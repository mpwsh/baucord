# baucord — the road to production

Written 2026-10-07 at 1.1.7, from a devil's-advocate review of the
whole repository in five parts (server, protocol and transport; client
and app logic; audio and video; the phone hosts; build, release and
licensing), with the serious claims re-checked by hand. Refreshed
2026-10-09 at 1.2.0: every finished item left the lists for a line in
§7. **This file is the one list of what is left.**

## 0. How to use this file

- Every item has an id (`SEC-3`), a priority, an effort, the evidence
  (file:line **as of 2026-10-07** — grep the symbol if lines moved), a
  fix sketch and what "done" means. P0 = must be fixed before the
  service is opened to strangers or a store; P1 = soon after; P2 = worth
  doing, or worth knowing.
- Effort: S = an afternoon, M = a few days, L = a week or more.
- When an item is done, take it out of its list and add one line to §7
  with the date; when one of the decisions in §6 is taken, write it
  there.
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
   server's memory, disk or connection slots** (SEC-2…SEC-5). Today the
   defence is that only friends hold invites.
2. **The desktop self-updater installs whatever the update bucket
   serves**, unsigned (UPD-1). Whoever gets the bucket key, the domain
   or a CI action gets code execution on every desktop that clicks
   Update.
3. **Nothing runs the tests automatically** — releases are
   compile-only (REL-1).
4. **Crashes in the field are invisible** (OPS-2).
5. **Malicious content from other users** can freeze or exhaust a
   client: image/GIF decode bombs, oversized video, unbounded audio
   buffers (CLI-1…CLI-3).
6. **Phones in the stores.** What Google Play's review asks of the app
   shipped in 1.2 (§7); what is left for Play is mostly Play Console
   work (PLAY-STORE.md) plus MOB-16 and MOB-17. The App Store is not
   decided; it would need MOB-2, MOB-3, MOB-6, the iOS half of MOB-5,
   and MOB-19.

## 2. Milestone A — safe to expose (do first)

### Server, protocol, transport

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
- [ ] **CLI-9 [P1][S] A device that will not come back should say
  so.** Since 2026-10-08 (§7) a side that failed to reopen is retried
  every 2 s (`input_pending`, `output_pending`), so a lost device no
  longer silences the session for good. Left: a `DeviceLost/Restored`
  event, and the mic button showing it.
- [ ] **CLI-11 [P0][S] Windows lowers other apps while baucord is in
  voice** (user reports, 1.1.7). The echo canceller's Communications
  streams trigger Windows' "reduce other sounds by 80 %" (AEC-SPEC
  §4.6). Since 2026-10-07 each voice stream asks Windows not to duck
  others (AEC-SPEC E11). Left: verify on Windows 11, speakers, Echo
  cancellation Auto: music playing, join voice — the music must keep
  its volume, and both `Native WASAPI capture/render:` lines read
  `do-not-duck=requested`. Windows 10 cannot be helped this way. If 11
  still ducks: the software canceller (AEC-SPEC §8) or Auto keeping
  speakers off the call path — decide then. Meanwhile users:
  `control mmsys.cpl,,3` → "Do nothing".
- [ ] **CLI-14 [P2][S] A stream's own audio output never sheds
  latency when it resamples.** Found reading `audio/src/stream/output.rs`
  for MOB-18: on a device that is not at 48 kHz, decoded audio waits in
  the worker's accumulator while the ring is full and is never
  trimmed, so a network burst or clock drift adds delay for good (the
  48 kHz path drops the excess instead). Rare — the output asks for
  48 kHz first — but it is an A/V drift. The `viewing` line's `audio
  … ms queued` shows it if it happens; drain to a target like the
  phones' mix does (`stream::mix`, by playing slightly faster).
- [ ] **CLI-16 [P2][M] Desktop voice latency: what is left.** The
  2026-10-08 review: native WASAPI cost nothing (event-driven, MMCSS,
  20 ms render buffer). Our code adds ~74 ms one way; the estimate
  mouth to ear is 135–175 ms. Nine candidates, ranked, in
  `LATENCY-SPEC.md`. First: measure (a clap test, and buffer depths
  in the voice summary). Then LAT-1 (late packets push playback 20 ms
  later), LAT-2 (share audio waits for the video tick) and LAT-3 (the
  32 KiB datagram send buffer holds seconds, not ~200 ms). Also seen:
  a sharer's own voice lost 45 % beside a collapsed video uplink
  (CLI-13's session, §7) — voice and video share the congested
  window, and LAT-3 is part of the answer. Parked until the Play
  release is out.
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

- [ ] **LEG-1 [P1][S] Pin the FFmpeg source.** Decided 2026-10-09
  (§6, 1; §7): a desktop build as a whole is GPL-3.0, and its notices
  say so and name the exact sources (LEG-3); x264 is pinned
  (`X264_COMMIT`). Left: checksum the FFmpeg tarball (REL-4), and
  configure FFmpeg with `--disable-everything` plus only what is used
  (also less attack surface).
- [ ] **LEG-4 [P1] H.264 patent licensing** for encoding/decoding on the
  desktop — a question for counsel, not code.
- [ ] **LEG-5 [P0 for stores][S] The stores' data forms.** The privacy
  policy, the terms and the account-deletion page are written, public
  and linked from the site's footer (`web/privacy.html`, `terms.html`,
  `delete-account.html`; signed 0x0 Studios, support@mpw.sh; §7). They
  cover the apps and the official server; other servers' operators
  answer for their own. Left: Play's Data safety form from
  PLAY-STORE.md §6; the official server's retention and whether
  telemetry needs consent (§6, 3); LEG-6, which would let the policy
  promise less.
- [ ] **LEG-6 [P1][S] What the privacy policy has to admit today.**
  Found writing it (2026-10-08). Each fix shortens a paragraph of
  `web/privacy.html`:
  - The server log writes the text of every chat message
    (`server.rs` ~1985, `[{username}]: {text}`), the names of files
    posted (`files.rs` ~238), who sends DMs to whom and invite codes.
    That is SEC-10; doing it removes a paragraph of the policy.
  - Uploaded client logs (`<data>/logs/`) are never deleted. Prune them
    after 30 days (the purge already takes a deleted account's).
  - The 90-day message prune (`store.rs` `prune`, `DELETE FROM
    messages`) does not lower the attachments' `refs`, so their files
    are kept for good. Release them in the same transaction.
  - The in-app note (`privacy-note`) says "Nothing goes anywhere else".
    On the desktop the update check and pasted image links do; nor does
    it say that the server keeps messages. Point it at the policy.
  - The deployed server writes its log to stderr; compose keeps
    5 × 10 MB of it, so its age depends on traffic. Keep it for a
    fixed time instead.
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
- [ ] **REL-3 [P1][M] Signing secrets are exposed to build scripts.**
  Android, since D62 (§7): builds without the key in its environment,
  signs in later steps, no persisted token — but on the same runner,
  so a build script could tamper with the signing tools, and
  `contents: write` is still workflow-wide. iOS (`ios-release.yml`) is
  unchanged. Fix: build without secrets, sign in a separate job,
  `contents: read`, `persist-credentials: false`, upload from a
  separate job.
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
  from the published `version` file. Since D55 the page also links the
  APK, which `android-release.yml` uploads on its own schedule: until
  it lands, the Android link 404s (minutes). Deploying the page after
  both workflows, or hiding a row whose HEAD fails, closes it.
- [ ] **REL-6 [P2] Supply chain and release hygiene**: no
  cargo-deny/audit; `serde_yaml` is deprecated; `atty` arrives through
  `nnnoiseless`'s default `bin` feature; Dependabot PRs merge with no
  CI; no Gradle wrapper checksum; floating NDK, cargo-ndk and runner
  images; no CHANGELOG; the `version` file can go up before its zip;
  the site promises SHA256SUMS that are never produced, "about 40 MB of
  RAM" (100–200 MB on macOS/Linux) and "a desktop app"; Google Fonts are
  loaded from Google (privacy).
- [ ] **OPS-1 [P1][S] Deployment leftovers.** The drift itself is
  fixed (2026-10-08, §7). Left: `config/baucord.yaml` is still the
  official server's own, committed with its public IP, port 5000 and a
  1.0 room password. Take it out of the repo once the host has its own
  copy (a pull that deletes it would break the bind mount, and a
  changed channel list applies live). Also: no `HEALTHCHECK`, no
  `read_only`/`cap_drop`/`no-new-privileges`, base images not pinned
  by digest, amd64 only.
- [ ] **OPS-2 [P1][M] Crash reporting.** Release builds abort on panic
  and write the hook's lines to a local log; native crashes (Skia,
  AAudio, JNI, FFmpeg) leave nothing a user can reach; Android symbols
  are not kept; iOS Ad Hoc builds never reach Xcode's Organizer. Either
  a minidump/Sentry-style reporter (opt-in), or at minimum: symbol
  archives per release, the build id and load address in every log,
  and the Debug tab's "Send log" after a crash.
- [ ] **OPS-3 [P2] Metrics, backups, keys.** A `metrics:` line or a
  Prometheus endpoint from `LinkStats` and the relay stats; a cheap
  health probe (`server --check`); scheduled `server backup`; how long
  a retired identity key keeps vouching (2 years today) and a
  rotation runbook beside `setup.sh`.

## 4. Milestone C — phones in the stores

Today's builds are a sideloaded APK (ANDROID-SPEC D4) and Ad Hoc builds
(IOS-SPEC D37; D10 planned TestFlight). **Google Play is the goal**
(§6, 2): **PLAY-STORE.md** is the process — account, signing, forms,
the 14-day closed test. What the app itself had to do for it shipped
in 1.2 (§7); left here: MOB-5's Play side, MOB-16, MOB-17. The App
Store is not decided yet.

- [ ] **MOB-2 [P0][S] iOS privacy manifest.** No `PrivacyInfo.xcprivacy`;
  the binary uses required-reason APIs (file timestamps through
  `std::fs::metadata`, `mach_absolute_time` through cpal). App Store
  Connect refuses uploads without it, TestFlight included.
- [ ] **MOB-3 [P0][S] The iOS icon keeps its alpha channel**
  (`ios/scripts/render_icon.sh`, on purpose for development); App
  Store Connect rejects it. Flatten onto #1A1D21 for release builds.
- [ ] **MOB-5 [P0][M] Store pipelines.** Android is done (§7;
  ANDROID-SPEC D62). Left: on Play's side, give it our release key as
  the app signing key, which is also the upload key (PLAY-STORE.md §4,
  the user); later, upload from CI through the Play Developer API. iOS
  exports Ad Hoc only (`ios-release.yml` ~148, ~231): add the App
  Store/TestFlight export and the symbol upload.
- [ ] **MOB-6 [P1][S] Encryption export compliance.** No
  `ITSAppUsesNonExemptEncryption` in the Info.plist; rustls/ring are
  non-Apple cryptography (a BIS self-classification is likely). Play:
  nothing to file (checked 2026-10-08, PLAY-STORE.md §10). Standard
  crypto in a free mass-market app is 5D992.c, and since BIS's
  2021-03-29 rule it needs no report; the email notification is only
  for non-standard crypto.
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
  the reconnect timer fires; neither phone watches connectivity (§7 in
  both specs); QUIC migration covers small moves. Keep the audio unit
  running (silence) or a background task while reconnecting; a
  connectivity monitor that reconnects on a new network.
- [ ] **MOB-13 [P2] Smaller phone items**: the service notification
  says "Connected" during a reconnect and has no Leave/Mute actions;
  Android does not persist settings on `Stop` (iOS does); iOS build
  numbers come from two counters (commit count locally, run number in
  CI); the predictive-back opt-out (D29) is one Google warns may
  misbehave "in a future release" (no date given; PRD-7);
  keys and settings are plain files (Keystore/Keychain later); the iOS
  log sits in `Documents` (visible in Files, backed up) and holds
  invite codes (SEC-10).
- [ ] **MOB-19 [P1][M] What the iPhone port left undone** (IOS-SPEC
  §4.3, §4.4, §6.2, §6.4, §7, from its first plan): the permission
  strings in Spanish and Portuguese (`InfoPlist.strings`); asking for
  the microphone at the first join (`AVAudioApplication
  requestRecordPermission`) with a link to Settings on a refusal, and
  a connect error that names a refused local-network permission;
  activating the audio session on joining a room and deactivating it
  with `notifyOthersOnDeactivation` on leaving (a podcast resumes);
  the route control (Settings lists cpal's devices on iOS today; the
  plan maps `AVAudioSessionPort`s onto `audio::route`); DSCP through
  `SO_NET_SERVICE_TYPE` for both address families; dropping the
  image caches on a memory warning.
- [ ] **MOB-16 [P0][S] The Play listing.** Texts (title ≤ 30, short
  ≤ 80, full ≤ 4,000 characters) in en, es-AR, pt-BR; icon 512 × 512;
  feature graphic 1024 × 500; 4+ phone screenshots at 1080 × 1920
  rendered from the UI with demo data.
- [ ] **MOB-17 [P0][S] Reviewer access.** A long-lived `member` invite
  on the official server, a channel with history, written steps for
  Play's App access section; the server kept up through every review.

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
  accept/reject dialog; connection-quality indicator; per-channel
  speaking indicators (today global); paste an invite string on the
  connect screen; `ServerLimits` on the wire (grey the `+` at the cap);
  scroll anchoring on "load earlier" (the view stays near the top
  but not on the message it showed); arrow keys in the context menu;
  `PreferredAddress` (move clients off an old port); the audio ladder
  holding 32 kbps under flat-RTT loss (RED ×3 instead of stepping
  down); quinn's ack-frequency for share viewers (~500 ACKs/s upstream
  today); the video group's finish-ack deadline (500 ms + 2·RTT),
  tuned once `sharing` lines show which cause abandons groups;
  timestamps on stream audio (real A/V sync); telemetry additions;
  mimalloc for memory retention on macOS/Linux (measure first,
  SESSION-GUIDE §4); simulcast or per-viewer rungs; a relay keyframe
  cache; hardware decode on the desktop; hot-reload beyond
  `channels`; channel topics, reserved names, an away toggle
  (PERSISTENCE-SPEC §11 "later").
- [ ] **PRD-7 The Android 16 predictive-back API** and the mobile
  `.slint` root (ANDROID-SPEC §9) when the shared one stops fitting.
- [ ] **PRD-8 [P2] Rough edges** (from SESSION-GUIDE's old §6): dialog
  `Field`s have no explicit select-all; the hover cursor over links is
  blocked under a `StyledText`; `profile_locked` on the user row is
  unused (the lock is the restriction); the string `chat-dm-received`
  and the `Tr` properties `ptt-tooltip`,
  `audio-devices-header`, `audio-system-default`, `keybinds-header`,
  `keybind-record` are unused (the first two messages are read with
  `t!`, the properties are not). The chat's horizontal scrollbar stays
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
  backend. (The chat pane's scrolling has its harness,
  `tools/chat-scroll-sim`.)
- [ ] **TST-3 [P2][M] A load scenario in the simulator**: 200 clients,
  5 sharers, mixed links; assert voice loss and RTT spread.
- [ ] **TST-4 [P1] Device passes the sandbox cannot do**, before each
  phone release: Android 10, 11, 14 and 16; an older and a current
  iPhone; Bluetooth headsets; a phone call mid-session; Wi-Fi ↔
  cellular; screen off for 10 minutes; the notifications. On
  the desktop: watched keybinds per OS, the single-instance guard, the
  tray, `server restore` on a real backup, an image over 10 MiB.

## 6. Decisions only you can take

1. **Licences** (LEG-1, LEG-2) — *decided 2026-10-09*: baucord stays
   Apache-2.0; a desktop build as a whole is GPL-3.0 (FFmpeg and x264);
   Slint under its royalty-free licence.
2. **Stores** — *Google Play decided 2026-10-07*: yes, PLAY-STORE.md
   (its §2 holds the decisions it needs). The App Store: still open.
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

One line each; the spec decision named holds the detail.

- **2026-10-07 — SEC-1**: quinn-proto 0.11.17 → 0.11.19, which fixes a
  remote panic. The old version's datagram workaround (`drop = false`)
  goes with LAT-3.
- **2026-10-07 — notifications on the phones** (ANDROID-SPEC D52,
  IOS-SPEC D50), only while the app stays connected (PRD-1).
- **2026-10-07 — MOB-1**: no microphone in the lobby or while muted,
  on the phones (ANDROID-SPEC D53, IOS-SPEC D51).
- **2026-10-07 — Android release CI** on maintained actions, able to
  rebuild a release by its tag (ANDROID-SPEC D54).
- **2026-10-07 — the documents**: START-HERE.md and this plan, which
  replaced `todo.md`, the `todos` note and SESSION-GUIDE's diary.
- **2026-10-08 — MOB-12**: a watched stream plays through the call
  path on the phones (ANDROID-SPEC D56, IOS-SPEC D52).
- **2026-10-08 — Settings → Debug → Copy log**, and a clipboard on the
  phones (ANDROID-SPEC D58, IOS-SPEC D53).
- **2026-10-08 — MOB-18**: the phones keep up with a 60 fps stream —
  only the shown picture is converted, over several cores (D57), and
  MediaCodec's pictures are paced (D59); the Galaxy A07 then showed
  ~60 fps at 1024x768 and ~58 at 1080p.
- **2026-10-08 — stream logs**: one `viewing …` / `sharing …` line per
  stream every 5 s (SESSION-GUIDE §4 reads them).
- **2026-10-08 — CLI-13**: the video ladder steps down on a collapsed
  uplink (`client/src/screen.rs` `LinkSample`).
- **2026-10-08 — CLI-12, and CLI-9's retries**: Windows asks cpal only
  when the native stream will not open; a side that is not open is
  retried every 2 s.
- **2026-10-08 — CLI-15**: a frame too large to send raises the
  encoder's quantizer floor for the next session; the next AMD log had
  one such frame and none after. If they come back: FFmpeg 8's amfenc
  forces IDRs without a new session, or a 2-byte chunk count (wire v4)
  lifts the 275 KB limit.
- **2026-10-08 — stream audio on the phones**: stereo to mono without
  cancelling (ANDROID-SPEC D60), a backlog drained by playing faster
  (D61).
- **2026-10-08 — MOB-5, Android**: the Play bundle beside the APK, both
  signed after the build (ANDROID-SPEC D62; REL-3 in part).
- **2026-10-08 — OPS-1's drift**: the hosting guide on the website
  (`web/deploy.html`), deploy files that match the readme,
  `config/baucord.example.yaml` with a test, `/deploy/` on the site.
- **2026-10-09 — 1.2.0, what the stores require**: MOB-4 report and
  block, MOB-14 delete my account (PERSISTENCE-SPEC §12 S1), MOB-15
  the terms on the connect card, MOB-11 the foreground service typed
  for Play (ANDROID-SPEC D63), LEG-5's pages (`web/privacy.html`,
  `terms.html`, `delete-account.html`).
- **2026-10-09 — licences**: LEG-1 decided (a GPL-3.0 desktop build,
  x264 pinned); LEG-2, Slint under its royalty-free licence with
  `AboutSlint` in Settings → About; LEG-3, third-party notices in
  Settings → Licenses and `web/licenses.html`, refreshed by CI before
  every build and deploy (`licenses.yml`); every crate declares
  `license = "Apache-2.0"`.
- **2026-10-09 — 1.2.0, the rest**: `Authors` (a lobby poster's name,
  offline authors' looks), the chat's stickiness by input with an
  "N new messages" pill (ANDROID-SPEC D66), General's Activity view
  (D65).

## 8. Facts this plan rests on

- **Tests, 2026-10-09, Linux sandbox**: protocol 86 + the versions
  test, transport 45, server 51 unit + 88 e2e — all green; the chat
  pane's 48 scrolling checks (`tools/chat-scroll-sim`) pass.
  Windows-only code (`wasapi.rs`, DXGI) is not compiler-checked here.
  Android: the Java compiles against API 33's real `android.jar`, the
  Rust JNI code type-checks in a scratch crate; iOS: the app
  type-checks with `-Zbuild-std` clippy. Nothing was run on a device.
- **The release pipeline** (every build first refreshes the
  third-party notices, `licenses.yml`):

  | platform | trigger | build | signing | users update by |
  |---|---|---|---|---|
  | desktop (win-x64, macOS arm64/intel, linux-x64) | release published | GitHub matrix, static FFmpeg 7.1, `--profile deploy --features tray`, link checks | none | in-app updater (unsigned zip from R2) |
  | server image | release published | buildx, amd64, distroless, uid 10000 | none | `docker compose pull` |
  | Android | release published / manual | cargo-ndk + Gradle `assembleRelease bundleRelease`, arm64, 16 KB check | the release key, after the build (APK and AAB) | sideloading the APK; Play once listed |
  | iOS | release published / manual | macos-26, Xcode, xcodegen, archive | Ad Hoc profile + certificate | reinstalling the `.ipa` |
  | website | `web/**` or the notices' inputs on main, or after any build | stamps the desktop version | — | — |
