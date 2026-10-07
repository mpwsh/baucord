# baucord — the road to production

Written 2026-10-07 at 1.1.7, from a devil's-advocate review of the
whole repository in five parts (server, protocol and transport; client
and app logic; audio and video; the phone hosts; build, release and
licensing), with the serious claims re-checked by hand. It replaces
`todo.md` (2026-09-03) and the `todos` note, and SESSION-GUIDE.md's old
"Remaining work" diary: **this file is the one list of what is left.**

## 0. How to use this file

- Every item has an id (`SEC-3`), a priority, an effort, the evidence
  (file:line **as of 2026-10-07** — grep the symbol if lines moved), a
  fix sketch and what "done" means. P0 = must be fixed before the
  service is opened to strangers or a store; P1 = soon after; P2 = worth
  doing, or worth knowing.
- Effort: S = an afternoon, M = a few days, L = a week or more.
- When an item is done, move it to §7 with the date and a one-line
  note; when one of the decisions in §6 is taken, write it there.
- Items marked *(review)* were found by reading code and not reproduced;
  confirm the failure before a large fix.

## 1. The verdict

The core is good: a sans-IO transport tested on a simulated network,
bounded media paths, a relay that never decodes media, a persistence
thread the tick never waits on, honest platform audio (echo
cancellation verified on the stream actually opened), and careful
mobile work. The test suites are green (§8).

What stands between this and "production for strangers":

1. **Any client — some even before logging in — can exhaust the
   server's memory, disk or connection slots** (SEC-2…SEC-5; the remote
   crash, SEC-1, is fixed). Today the defence is that only friends hold
   invites.
2. **The desktop self-updater installs whatever the update bucket
   serves**, unsigned (UPD-1). Whoever gets the bucket key, the domain
   or a CI action gets code execution on every desktop that clicks
   Update.
3. **Licensing of the shipped binaries is unresolved**: FFmpeg is built
   with `--enable-gpl` and x264 is linked statically into an
   Apache-2.0 app with no notices; Slint's royalty-free licence asks
   for attribution that is not shown (LEG-1…LEG-3).
4. **Nothing runs the tests automatically** — releases are
   compile-only (REL-1).
5. **Crashes in the field are invisible** (OPS-2).
6. **Phones**: store submission needs several items (MOB-2…MOB-6),
   report/block above all. (The microphone running in the lobby and
   while muted, MOB-1, is fixed.)
7. **Malicious content from other users** can freeze or exhaust a
   client: image/GIF decode bombs, oversized video, unbounded audio
   buffers (CLI-1…CLI-3).

## 2. Milestone A — safe to expose (do first)

### Server, protocol, transport

- [x] **SEC-1 [P0][S] Remote panic in quinn-proto 0.11.17.** Done
  2026-10-07 (§7): 0.11.19 fixes a `clamp` that panics when a peer's
  `min_ack_delay` exceeds the RTT, which our ack-frequency config
  reaches right after the handshake — with `panic = "abort"` that was
  a crash any unauthenticated peer could trigger. Follow-up (S): drop
  the datagram workaround the old version needed (`driver.rs` ~1359)
  once confirmed unnecessary.
- [ ] **SEC-2 [P0][M] Connection slots can be exhausted from one
  address.** `peer_count() >= max_peers` refuses new connections and
  counts handshaking, Hello-pending and closing peers alike
  (`transport/src/driver.rs` ~687, ~817); a connection that never says
  Hello lives 10 s (`server.rs` ~90, ~913); the per-IP limit only
  counts failed invites. ~26 connections/s fill the 256 slots of the
  production config (`max_clients`; the default 64 fills at ~6/s). Fix: a
  separate small budget for unadmitted connections, a per-IP
  concurrent cap and rate, QUIC `Retry` above a threshold, a shorter
  Hello deadline. Done when a sim test with one hostile address
  opening connections in a loop leaves room for a legitimate client.
- [ ] **SEC-3 [P0][M] No output caps and few rate limits on control
  messages.** Outgoing control/signal queues have no byte or age bound
  (`driver.rs` ~954); `FetchHistory` (up to 200 messages per call,
  `history.rs` ~128), `FetchBlob` (no permission check, copies the
  whole blob per request, duplicate fetchers accumulate —
  `profiles.rs` ~581–668), `Speaking`/`AudioState`/`ScreenShareStop`
  (broadcast to everyone, `server.rs` ~2092) are unlimited. A client
  that stops reading its stream pins the replies in server memory.
  Fix: per-session token buckets per message class (drop and count,
  disconnect on sustained abuse); cap queued control bytes per peer
  (close past ~2 MB or 5 s); coalesce speaking/audio state per tick;
  `FetchBlob` behind a permission, one fetch per hash, frames encoded
  once and shared. Done when e2e tests prove each limit.
- [ ] **SEC-4 [P0][S] A stalled video stream grows server memory without
  bound.** Items for stream N+1 queue while stream N stays untagged
  (`driver.rs` ~1761–1800, ~1723) and reading continues, returning flow
  credit. Fix: cap bytes per stream and per peer, drop a stream untagged
  after ~1 s, or stop reading while blocked. Sim test.
- [ ] **SEC-5 [P0][M] Disk and memory fill through uploads, telemetry
  and logs.** Avatar-sized uploads skip permission, the `upload`
  restriction, quotas and transfer budgets and stay cached forever
  (`profiles.rs` ~326–348, ~505); `TelemetryEvent` has no rate limit
  and stores up to 64 KiB per row (`server.rs` ~2264); log uploads need
  no permission, their 10-minute limit is keyed by session and
  forgotten on reconnect, the folder has no cap and the write happens
  on the tick (`server.rs` ~1577–1625). Fix: avatars need `UPLOAD` and
  a per-user quota and rate, cache only referenced blobs; truncate and
  rate-limit telemetry per user; key log uploads by user, cap the
  folder, write off the tick.
- [ ] **SEC-6 [P1][S] No per-sender packet rate on the voice relay**
  (`relay.rs` ~123–163, guests can speak). A token bucket (~100
  packets/s) before fan-out, system audio too.
- [ ] **SEC-7 [P1][S] Chat images are copied once per recipient** in
  `send_media` (`driver.rs` ~1085): a 10 MiB image to 256 clients holds
  ~2.5 GiB. Write the length prefix separately and share the `Bytes`;
  cap queued media bytes per peer.
- [ ] **SEC-8 [P1][S] Members can mint unlimited-use invites at will**
  (`server.rs` ~2167–2214; members hold `CREATE_INVITES`). Cap
  `max_uses` and live invites per member, rate-limit new users per
  address and globally, prune expired invites.
- [ ] **SEC-9 [P1][S] The persistence queue can be starved, and a dropped
  write logs its `Debug`** — a `StoreBlob` prints its bytes (up to
  50 MB → a ~200 MB log line on the tick), an `InsertInvite` prints a
  live code (`persistence/mod.rs` ~673–686). A priority lane for policy
  writes (bans, users, invites), a redacted `Debug`, a rate-limited
  warning; DM reads separated from history spam.
- [ ] **SEC-10 [P1][S] The logs hold private data and grow without
  bound.** Every General message verbatim at info (`server.rs` ~1985),
  DM sender/recipient, invite codes (server ~1162, ~2201; client
  `network.rs` ~1504 — and the client log can be uploaded), one `link:`
  line per client every 2 s by default, client-triggerable lines with
  no rate limit, newlines in chat that forge log lines; the logger
  writes synchronously on the tick; compose sets no log rotation.
  Stop logging content and codes, default `link_stats_interval_secs`
  to 0 (on when diagnosing), rate-limit per-client lines, escape
  newlines, `logging: max-size` in compose.
- [ ] **SEC-11 [P2] Smaller server items** *(review)*: channel passwords
  plain, compared in variable time, no attempt limit (`server.rs`
  ~2661); a moderator can lift their own mute/restriction
  (`moderation.rs` ~432); admin replies over 64 KiB fail silently
  (ListUsers/Blobs/Invites/audit break at a few hundred rows,
  `messages.rs` ~1440, `server.rs` ~1310); pruning messages never
  decrements blob refs, the telemetry prune scans without an index,
  audit rows and invites are never pruned; a CLI write reloads the
  policy and can drop a ban still queued (`persistence/mod.rs` ~1007,
  `server.rs` ~1363); wall time is anchored once at start
  (`clock.rs`); shutdown can outlast Docker's 10 s stop timeout;
  `server restore` joins archive paths without `unpack_in`;
  `SSLKEYLOGFILE` is honoured (`identity.rs` ~449); look-alike and
  zero-width names pass; creating a channel past the cap stores it
  before failing.

### Client

- [ ] **CLI-1 [P0][M] Picture decode bombs freeze every client.**
  Received images decode on the UI thread with limits of 16384² and
  512 MiB (`app-core/src/images.rs` ~190); GIF frames allocate a full
  canvas each (`app-core/src/gif.rs` ~178–215, from `media.rs` ~198); the 800 px GIF cap is only on the
  sender; history replays the picture to every newcomer; avatars use
  the same path. A long UI stall backs up `route_message` and can drop
  the connection. Fix: read the header first and refuse stills over
  ~4096 px and GIF canvases over ~800 px, `max_alloc` ~64 MB, decode
  off the UI thread; check dimensions on the server too.
- [ ] **CLI-2 [P0][S] Oversized video from a malicious sharer.** The
  FFmpeg decoder has no `max_pixels` and builds a scaler and an RGBA
  frame at whatever size the SPS says (`video/src/codec.rs` ~620–746).
  Set `max_pixels` (3840×2160), reject frames above it or above twice
  the announced size. Same check for MediaCodec/VideoToolbox outputs.
- [ ] **CLI-3 [P1][S] The audio packet buffer has no slot cap** and its
  time cap trusts sender timestamps (`audio/src/transport/
  packet_buffer.rs` ~192–262), on the real-time output worker. Cap ~64
  slots per peer and refuse sequence numbers far ahead of the playhead.
- [ ] **CLI-4 [P1][S] A message can carry a link of any scheme.** The
  linkifier puts the raw token into `](<…>)` unescaped
  (`app-core/src/markdown.rs` ~116–134, ~242), so `>` ends the
  destination early and a second `[text](file://…)` follows; the click
  handler opens anything (`frontend/src/app.rs` ~1178 → `host.rs` ~95;
  `open::that_detached`). Allow only http/https at the click, stop the
  URL at `<>[]()`, add adversarial tests. (Phones already check the
  scheme in their hosts.)
- [ ] **CLI-5 [P1][S] Reconnect unmutes.** `teardown_connection` clears
  mute and deafen (`frontend/src/app/session.rs` ~196) and the
  automatic reconnect puts the user back in the room live. Keep both
  across reconnects and re-apply after `VoiceControl::Start`.
- [ ] **CLI-6 [P1][S] `settings.json` is written in place**
  (`client/src/config.rs` `Settings::save`) and a parse failure falls
  back to defaults silently, so a crash mid-write loses every server,
  password and pin (the next connect trusts on first use again). Write
  to a temporary file and rename, keep a `.bak`, log and show parse
  errors.
- [ ] **CLI-7 [P1][S–M] The identity key is easy to lose.** Release
  builds keep the config beside the executable when it is writable, so
  unpacking the next release elsewhere makes a new user; import
  overwrites the key without a backup and with default permissions.
  Use the platform folder unless a `portable` marker exists; back up
  before import; 0600.
- [ ] **CLI-8 [P1][S] Received files open with one click.** Names keep
  bidi controls (`invoice‮fdp.exe`); Open runs the OS handler; no
  Mark-of-the-Web or quarantine. Strip format characters, confirm (or
  refuse) executables and scripts, tag downloads on Windows/macOS.
- [ ] **CLI-9 [P1][S] One failed device reopen silences the mic or the
  speaker for the session** (`audio/src/lib.rs` ~1106, ~1144, ~959;
  `client/src/voice.rs` ~547 ignores `service()`). Pending flags for
  both sides on any failed reopen, a `DeviceLost/Restored` event, the
  mic button shows it.
- [ ] **CLI-10 [P2] Smaller client items** *(review)*: auto-reconnect
  gives up after ~60 s (5 tries) — phones on a train need longer;
  server addresses must be IP literals (no DNS — also an App Review
  problem on Apple's IPv6-only network); the desktop log lives in the
  working directory and is never rotated; start-minimized plus an
  update prompt never connects; `percent_decode` slices on bytes and
  panics on a pasted `file:///%aé` (`images.rs` ~331); `with_app` is a
  plain `borrow_mut` that a synchronous Slint callback could re-enter;
  chat memory is bounded by rows, not bytes; release symbols are
  stripped so panic backtraces have no names.

### Updates

- [ ] **UPD-1 [P0][M] Sign what the updater installs.** It fetches
  `baucord.com/version` and the zip and installs it with no hash or
  signature check (`frontend/src/updater.rs` ~168–325); the release
  job uploads with a bucket-wide key through an unpinned third-party
  action (`release.yml` ~443, `web.yaml` ~26). Sign a manifest
  (version + SHA-256 per zip) with an offline ed25519/minisign key,
  embed the public key, verify before extracting, `https_only`;
  scope the bucket token to release objects; replace the action with
  a pinned `aws s3 cp`.
- [ ] **UPD-2 [P1][S] Downloads longer than 30 s always fail**
  (`timeout_global(30 s)` covers the body, `updater.rs` ~185; the zips
  are tens of MB). Connect and idle timeouts instead, retry/resume,
  temp file + rename; the companion binary is replaced after the main
  one, so a failure leaves a half-update.
- [ ] **UPD-3 [P1][S] Pre-releases reach every user**: every workflow
  fires on `release: published`, which includes pre-releases; the R2
  `version` file and Docker `latest` move too. Guard on
  `github.event.release.prerelease`.

## 3. Milestone B — release integrity, licensing, operations

- [ ] **LEG-1 [P0][M] FFmpeg and x264 are GPL in an Apache-2.0
  binary.** Linux/macOS FFmpeg is configured `--enable-gpl
  --enable-libx264` and linked statically (`release.yml` ~173, ~240);
  Windows uses a `…x264-gpl…` build (`release.yml` ~271,
  `video/build.rs` ~30); x264 is cloned at HEAD (the exact source
  cannot be named); the zips carry no licence text. Mobile builds do
  not link FFmpeg. Either (a) an LGPL FFmpeg without x264 — hardware
  encoders, `h264_videotoolbox` on macOS (compiled in but not in
  `ENCODER_PRIORITY`, `codec.rs` ~93), OpenH264 as the software
  fallback — or (b) ship the desktop binary as GPLv3 with notices and
  a source offer. Either way: pin the x264 commit, checksum the FFmpeg
  tarball, configure FFmpeg with `--disable-everything` plus only what
  is used (also less attack surface). Decision: §6, 1.
- [ ] **LEG-2 [P0][S] Slint's licence terms.** Slint is GPLv3,
  royalty-free (attribution: the `AboutSlint` widget in an About
  screen, or the "Made with Slint" badge where the app is downloaded)
  or commercial. Neither attribution exists (About lists "Slint" as
  text, `settings.slint` AboutPanel; `web/index.html`). Pick the licence
  (§6, 1) and add `AboutSlint {}` to the About panel and the badge to
  the site.
- [ ] **LEG-3 [P0][S] No third-party notices anywhere.** Generate them
  (cargo-about, plus FFmpeg/x264/Skia/Opus texts), ship them in every
  zip, APK and IPA, and show them in About.
- [ ] **LEG-4 [P1] H.264 patent licensing** for encoding/decoding on the
  desktop — a question for counsel, not code.
- [ ] **LEG-5 [P0 for stores][S–M] A privacy policy and the stores' data
  forms.** The app sends voice, messages, files, a device key,
  telemetry every 10 s and (on request) logs. Who the controller is when
  anyone can host a server needs a deliberate answer; the official
  server's retention needs writing down; telemetry may need consent.
- [ ] **REL-1 [P0][S–M] A CI gate.** `ci.yaml` and `pr-check.yaml` are
  `.disabled`; `release.yml` runs `cargo build` only, without
  `--locked`; the versions test (`protocol/tests/versions.rs`) never
  runs in CI, and the release guard checks only `crates/desktop`
  against the tag while the updater compares `frontend`'s version.
  Re-enable a PR job (`cargo test --locked --workspace`, clippy
  `-D warnings` on the host crates, nightly `fmt --check`, the versions
  test) and make the release jobs `needs:` it.
- [ ] **REL-2 [P1][M] Code signing**: Windows (Azure Trusted Signing or an
  OV certificate) and macOS (Developer ID, a real `.app` with an
  `Info.plist` — which is also how macOS attributes the microphone and
  screen-recording prompts — and notarization). Unsigned self-replacing
  executables trip SmartScreen, Gatekeeper and antivirus heuristics.
- [ ] **REL-3 [P1][M] Signing secrets are exposed to every build
  script.** The Android keystore and passwords sit in the environment
  of the step that runs cargo-ndk and every `build.rs`
  (`android-release.yml` ~37, ~112); both mobile workflows grant
  `contents: write` workflow-wide and `checkout` persists the token.
  Build without secrets, sign in a last step, `contents: read`,
  `persist-credentials: false`, upload from a separate job.
- [ ] **REL-4 [P1][S] Pin and verify build inputs.** Actions by SHA
  (`dtolnay/rust-toolchain@stable` is a branch), top-level
  `permissions: {}`, SHA-256 for the FFmpeg, PipeWire and x264 sources
  and the Windows FFmpeg zip on R2 (its builder workflow is disabled and
  names a different object). Skia's prebuilt download is unverified
  upstream — note it.
- [ ] **REL-5 [P1][S] The website can advertise versions that do not
  exist yet**: `web.yaml` runs after any successful "Build and
  Release", including the cache-seeding pushes to `chunk`, and stamps
  that branch's version. Run only for releases, or read the version
  from the published `version` file.
- [ ] **REL-6 [P2] Supply chain and release hygiene**: no
  cargo-deny/audit; `serde_yaml` is deprecated; `atty` arrives through
  `nnnoiseless`'s default `bin` feature; Dependabot PRs merge with no
  CI; no Gradle wrapper checksum; floating NDK, cargo-ndk and runner
  images; no CHANGELOG; the `version` file can go up before its zip;
  the site promises SHA256SUMS that are never produced, "about 40 MB of
  RAM" (100–200 MB on macOS/Linux) and "a desktop app"; Google Fonts are
  loaded from Google (privacy); no crate declares `license`.
- [ ] **OPS-1 [P1][S] Deployment drift.** `scripts/setup.sh` requires
  `docker-compose.yml` but the repo has `docker-compose.yaml` (it exits
  at once); it greps for an `Invite:` line the server never prints;
  the readme says UDP 51000 while the committed compose and
  `config/baucord.yaml` use 5000; that config commits a public IP and
  a 1.0 room password ignored since 1.1. Also: no `HEALTHCHECK`, no
  log rotation, no `read_only`/`cap_drop`/`no-new-privileges`, base
  images not pinned by digest, amd64 only.
- [ ] **OPS-2 [P1][M] Crash reporting.** Release builds abort on panic
  and write the hook's lines to a local log; native crashes (Skia,
  AAudio, JNI, FFmpeg) leave nothing a user can reach; Android symbols
  are not kept; iOS Ad Hoc builds never reach Xcode's Organizer. Either
  a minidump/Sentry-style reporter (opt-in), or at minimum: symbol
  archives per release, the build id and load address in every log,
  and the Debug tab's "Send log" after a crash.
- [ ] **OPS-3 [P2] Metrics and backups.** A `metrics:` line or a
  Prometheus endpoint from `LinkStats` and the relay stats; a cheap
  health probe (`server --check`); scheduled `server backup`.

## 4. Milestone C — phones in the stores

Today's plan says sideloaded APK (ANDROID-SPEC D4) and Ad Hoc builds
(IOS-SPEC D37; D10 planned TestFlight). If the stores become the goal (§6, 2), these block it.

- [x] **MOB-1 [P0][M] The microphone runs in the lobby.** Done
  2026-10-07 (§7; ANDROID-SPEC D53, IOS-SPEC D51) — what remains is the
  Play/App Store side: the Android service still declares the
  microphone type for the whole session (needed for a background
  rejoin), and iOS keeps a silent output in the lobby to stay alive.
  The original finding: every connect
  starts the voice engine (`frontend/src/app/presence.rs` ~70) and the
  keep-alive (Android's *microphone* foreground service, iOS's
  background audio), though the server drops lobby voice. Capture
  shows the system's microphone indicator, uploads silence the server
  discards, costs battery, and is hard to justify to Play (FGS policy)
  and Apple (guideline 2.5.4). Start capture, the service type and the
  audio session on joining a room; in the lobby keep only what keeps
  the connection (which Android service type then applies needs
  thought; §6, 5). Interacts with the first-joiner notice,
  which relies on staying connected.
- [ ] **MOB-2 [P0][S] iOS privacy manifest.** No `PrivacyInfo.xcprivacy`;
  the binary uses required-reason APIs (file timestamps through
  `std::fs::metadata`, `mach_absolute_time` through cpal). App Store
  Connect refuses uploads without it, TestFlight included.
- [ ] **MOB-3 [P0][S] The iOS icon keeps its alpha channel**
  (`ios/scripts/render_icon.sh`, on purpose for development); App
  Store Connect rejects it. Flatten onto #1A1D21 for release builds.
- [ ] **MOB-4 [P0][M–L] Report and block.** Both stores require them
  for user-generated content (App Store 1.2; Play's UGC policy), and
  Apple asks for account deletion where accounts are created — here a
  device key registered by invite. Protocol (a report message, a
  per-user block list), server (a moderator queue in the admin panel)
  and UI.
- [ ] **MOB-5 [P0][M] Store pipelines.** Play wants an AAB (the workflow
  builds an APK) and a decision on Play App Signing before the first
  upload (sideloaded installs cannot update from Play if the key
  changes); iOS exports Ad Hoc only (`ios-release.yml` ~148, ~231) —
  add App Store/TestFlight export and symbol upload.
- [ ] **MOB-6 [P1][S] Encryption export compliance.** No
  `ITSAppUsesNonExemptEncryption` in the Info.plist; rustls/ring are
  non-Apple cryptography (a BIS self-classification is likely).
- [ ] **MOB-7 [P1][S] Android moves the identity key to a new phone**:
  `allowBackup="false"` no longer covers device-to-device transfer for
  apps targeting 31+. Add `dataExtractionRules` excluding everything
  from cloud backup and device transfer (ANDROID-SPEC §8's intent).
- [ ] **MOB-8 [P1][S] The Activity being destroyed ends the call**
  (`android/src/lib.rs` exits the process): `configChanges` lacks
  `fontScale`, `fontWeightAdjustment`, `navigation`, `colorMode`, …
  (changing the font size mid-call kills it); on Android 10–11 Back on
  the main list finishes the Activity (add `onBackPressed →
  moveTaskToBack(true)`). Keeping the engine alive across Activities
  (D8) is the larger fix.
- [ ] **MOB-9 [P1][M] Phone calls and audio focus.** Android has no
  audio-focus or call-state handling (ANDROID-SPEC §6.5 planned it):
  the room plays over a cellular call and sends silence. iOS reopens
  only on an interruption's *ended*, which does not always come
  (`audio/src/native/avsession.rs` ~240, IOS-SPEC D47). Both: a
  "paused — call" state in the UI, resume on regain.
- [ ] **MOB-10 [P1][M] Losing the network in the background.** On iOS
  the engine stops 900 ms after a loss and the app is suspended before
  the reconnect timer fires; neither phone watches connectivity (M4 in
  both specs); QUIC migration covers small moves. Keep the audio unit
  running (silence) or a background task while reconnecting; a
  connectivity monitor that reconnects on a new network.
- [ ] **MOB-11 [P1][S] The Android foreground service.** A refused
  `startForeground` calls `stopSelf()` (`VoiceService.java`), which on
  a first start is the system's "did not call startForeground" crash,
  and on a reconnect stops the service keeping the app alive; a
  `PARTIAL_WAKE_LOCK` is held all session; Play needs the FGS type
  declarations (video) for `microphone|mediaPlayback`.
- [ ] **MOB-12 [P1][M] Stream audio on phones** ignores the chosen
  output and the echo canceller (`client/src/screen.rs` ~1580 spawns
  its decoder on the default device; AEC-SPEC E7/E10): a speaker user
  echoes the stream back. Route stream audio through the voice render
  mix. (Discussed 2026-10-07 and put on hold by the user.)
- [ ] **MOB-13 [P2] Smaller phone items**: the service notification
  says "Connected" during a reconnect and has no Leave/Mute actions;
  Android does not persist settings on `Stop` (iOS does); iOS build
  numbers come from two counters (commit count locally, run number in
  CI); the predictive-back opt-out will stop working at targetSdk 37;
  keys and settings are plain files (Keystore/Keychain later); the iOS
  log sits in `Documents` (visible in Files, backed up) and holds
  invite codes (SEC-10).

## 5. Milestone D — product

- [ ] **PRD-1 Push notifications** (FCM and APNs) — the only way to tell
  someone who is *not connected* that a DM arrived or a call started.
  Needs a gateway (the relay must not hold Apple/Google keys for every
  self-hosted server — likely an official push relay the servers call,
  opt-in per server), device-token registration over the protocol
  (new feature bit), per-user preferences, and the privacy text.
  Design doc first. Today's notifications work only while the app is
  connected (§7, 2026-10-07).
- [ ] **PRD-2 Replies/threads and reactions** on messages (from
  `todos`): wire variants under new feature bits, persistence columns,
  UI.
- [ ] **PRD-3 A feedback tab** (from `todos`): a text box plus "attach
  logs" (Settings → Debug's upload exists), stored on the server, an
  admin-panel tab to read them.
- [ ] **PRD-4 Desktop notifications** for DMs and mentions when the
  window is not focused (the phones' `notify` hook has no desktop
  twin yet; DM unread also clears in a background window — no focus
  tracking).
- [ ] **PRD-5 Platform gaps**: Wayland screen capture is xcap's
  screenshot fallback (a few fps), no Wayland detection; macOS capture
  goes through xcap's deprecated `CGWindowList` API (ScreenCaptureKit is
  the replacement); system audio in a share is
  a stub on macOS/Linux; no AEC on macOS (the VPIO backend is the
  answer, AEC-SPEC §9) or Linux; macOS encodes with x264 on the CPU;
  decode and encode share the screen thread.
- [ ] **PRD-6 Carried over from the old lists**: identity-changed
  accept/reject dialog; connection-quality indicator; push-to-talk
  (reserved in settings); per-channel speaking indicators (today
  global); paste an invite on the connect screen; `ServerLimits` on the
  wire (grey the `+` at the cap); scroll anchoring on "load earlier";
  arrow keys in the context menu; `PreferredAddress` (move clients off
  an old port); the audio ladder holding 32 kbps under flat-RTT loss
  (RED ×3 instead of stepping down); quinn's ack-frequency for share
  viewers (~500 ACKs/s upstream today); telemetry additions; mimalloc
  for memory retention on macOS/Linux (measure first, SESSION-GUIDE §4);
  simulcast or per-viewer rungs; a relay keyframe cache; hardware
  decode on the desktop; hot-reload beyond `channels`; channel topics, reserved
  names, an away toggle (PERSISTENCE-SPEC §11 "later").
- [ ] **PRD-7 The Android 16 predictive-back API** and the mobile
  `.slint` root (ANDROID-SPEC M2) when the shared one stops fitting.
- [ ] **PRD-8 [P2] Rough edges** (from SESSION-GUIDE's old §6): dialog
  `Field`s have no explicit select-all; the hover cursor over links is
  blocked under a `StyledText`; `profile_locked` on the user row is
  unused (the lock is the restriction); the string `chat-dm-received`
  and the `Tr` properties `ptt-tooltip`,
  `audio-devices-header`, `audio-system-default`, `keybinds-header`,
  `keybind-record` are unused. The chat's horizontal scrollbar stays
  `as-needed` on purpose: a 2 px thumb above the composer means a row
  inflated the list again (SESSION-GUIDE §4).

### Testing debt

- [ ] **TST-1 [P1][M] Fuzzing**: `protocol::frame::Decoder`,
  `transport::der`, `audio` RED decode, `video::annexb::split`, the
  packet buffer, the video reassembler, the decoders.
- [ ] **TST-2 [P1][M] The untested middle**: `client/src/network.rs`
  (3.2k lines, one idle test) — loss classification, reassembly
  limits, session phases; `frontend` has no tests — reconnect and
  backoff, mute across reconnect, updater extraction, settings
  corruption; `audio` device supervision (`service()`) against a mock
  backend.
- [ ] **TST-3 [P2][M] A load scenario in the simulator**: 200 clients,
  5 sharers, mixed links; assert voice loss and RTT spread.
- [ ] **TST-4 [P1] Device passes the sandbox cannot do**, before each
  phone release: Android 10, 11, 14 and 16; an older and a current
  iPhone; Bluetooth headsets; a phone call mid-session; Wi-Fi ↔
  cellular; screen off for 10 minutes; the notifications below. On
  the desktop: watched keybinds per OS, the single-instance guard, the
  tray, `server restore` on a real backup, an image over 10 MiB.

## 6. Decisions only you can take

1. **Licences** (LEG-1, LEG-2): LGPL FFmpeg without x264 or a GPLv3
   desktop binary; Slint royalty-free with attribution, GPLv3 or
   commercial.
2. **Stores**: are Google Play and the App Store goals for the next
   release, or stay with the sideloaded APK and Ad Hoc/TestFlight?
   That moves Milestone C up or down.
3. **Privacy** (LEG-5): what the official server keeps and for how
   long, telemetry consent, who answers for self-hosted servers.
4. **Push** (PRD-1): run an official push relay, or live with
   "notifications only while connected".
5. **The lobby** (MOB-1) — *decided 2026-10-07*: no microphone in the
   lobby or while muted, on the phones only; the desktop keeps its
   streams.
6. **Mute across reconnects and restarts** (CLI-5): assumed yes.
7. **The default branch**: `chunk` or `main` — the workflows disagree.

## 7. Done

- **2026-10-07 — SEC-1**: quinn-proto 0.11.17 → 0.11.19 (lockfile; the
  floor in `transport/Cargo.toml` raised with the reason). Protocol,
  transport and server suites green on it.
- **2026-10-07 — notifications on the phones** (ANDROID-SPEC D52,
  IOS-SPEC D50): the app's own icon on every Android notification (a
  silhouette of the dog for the status bar, the launcher icon beside
  it); the DM and mention chimes follow the phone's silent, vibrate and
  Do Not Disturb modes (iOS: the ring/silent switch, through the
  system); a DM while the app is in the background is a notification
  whose tap opens the conversation; "Ana joined General" when someone
  joins while every voice room was empty (at most every 10 minutes).
  All only while the app stays connected — PRD-1 for the rest.
- **2026-10-07 — MOB-1, the phones' microphone** (ANDROID-SPEC D53,
  IOS-SPEC D51): in the lobby Android opens no audio at all and iOS only
  a plain output (to stay alive in the background); muted or deafened in
  a room, Android closes the microphone and iOS mutes it on the voice
  unit (plain streams: closed). The desktop is unchanged. The voice
  thread ticks at 250 ms while no microphone is open.
- **2026-10-07 — the documents**: `START-HERE.md` (the way in, the
  reading order), this plan (replacing `todo.md`, `todos` and
  SESSION-GUIDE's diary), SESSION-GUIDE §1–§3 brought up to date (the
  sandbox now compiles and tests), status banners on the specs, the
  readme's features and limitations, ARCHITECTURE §7 (the hosts, audio
  and video per platform, notifications).

## 8. Facts this plan rests on

- **Tests, 2026-10-07, Linux sandbox**: protocol 84 + the versions
  test, transport 45, server 49 unit + 76 e2e — all green (also on
  quinn-proto 0.11.19); the touched `audio` and `client` tests green.
  Windows-only code (`wasapi.rs`, DXGI) is not compiler-checked here.
  Android: the Java compiles against API 33's real `android.jar`, the
  Rust JNI code type-checks in a scratch crate; iOS: the app
  type-checks with `-Zbuild-std` clippy. Nothing was run on a device.
- **The release pipeline**:

  | platform | trigger | build | signing | users update by |
  |---|---|---|---|---|
  | desktop (win-x64, macOS arm64/intel, linux-x64) | release published | GitHub matrix, static FFmpeg 7.1, `--profile deploy --features tray`, link checks | none | in-app updater (unsigned zip from R2) |
  | server image | release published | buildx, amd64, distroless, uid 10000 | none | `docker compose pull` |
  | Android | release published / manual | cargo-ndk + Gradle `assembleRelease`, arm64, 16 KB check | keystore from secrets | sideloading the APK |
  | iOS | release published / manual | macos-26, Xcode, xcodegen, archive | Ad Hoc profile + certificate | reinstalling the `.ipa` |
  | website | `web/**` on main, or after any build | stamps the desktop version | — | — |
