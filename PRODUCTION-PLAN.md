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
  address.** *In part, 2026-10-12 (review P3-5):* unadmitted
  connections have a budget of their own (a quarter of `max_clients`,
  16–64 seats), capped at eight per address (an IPv6 /64 counts as
  one), with a QUIC `Retry` once half the seats are taken;
  `one_address_cannot_fill_the_waiting_room` is the test asked for
  below. *2026-10-14:* a handshake that never finishes gives its seat
  back after 10 s (it held it for quinn's 30 s idle timeout), and a
  connection is counted under the address it arrived from, not one it
  migrated to before saying a word. *Left:* a rate per address, and
  the Hello deadline (still 10 s, for lossy links). The original finding: `peer_count() >= max_peers` refuses new connections and
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
  ~2.5 GiB. *In part, 2026-10-14:* the relay frames a picture once
  (`transport::frame_media`) and every receiver's stream queues the
  same bytes, which go to quinn by reference. *Left:* cap queued media
  bytes per peer.
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
  (`moderation.rs` ~432); admin replies are cut to one 17 KiB message
  since 1.2.1, so ListUsers, Blobs and Invites show only their first
  rows past about 140 entries (paging them needs a feature bit;
  review P0-1); pruning messages never
  decrements blob refs, the telemetry prune scans without an index,
  audit rows and invites are never pruned; look-alike and
  zero-width names pass; creating a channel past the cap stores it
  before failing. (Done 2026-10-14: the wall clock catches up after a
  pause, `server restore` takes only plain relative blob paths, and
  the TLS key log needs `BAUCORD_TLS_KEYLOG=1` beside `SSLKEYLOGFILE`.)

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
- [ ] **CLI-5 [P1][S] Reconnect unmutes.** `teardown_connection` clears
  mute and deafen (`frontend/src/app/session.rs` ~196) and the
  automatic reconnect puts the user back in the room live. Keep both
  across reconnects and re-apply after `VoiceControl::Start`.
- [ ] **CLI-6 [P1][S] `settings.json` is written in place** *(done
  2026-10-12 but for showing the parse error in the UI: it is logged,
  the file is kept as `settings.json.corrupt`, and the `.bak` loads.)*
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
  `control mmsys.cpl,,3` → "Do nothing", or on headphones Echo
  cancellation → Off. 2026-10-12 (AEC-SPEC E14, §4.6 table): plain
  streams no longer open through the Communications role (1.2.1 would
  have ducked headphone users on "Default"), baucord's own sessions
  opt out of being lowered, and the render line logs the user's
  Windows setting — ask a reporter for their two `Native WASAPI`
  lines. Windows 10 on speakers, decided 2026-10-12: keep the
  platform canceller and say so (AEC-SPEC E15) — Settings → Audio
  shows a notice with a button to Windows' Sound dialog whenever the
  opt-out was not granted and Windows is set to lower, and a toast
  points there once per install. Not ducking at all on Windows 10
  takes our own canceller on plain streams (AEC-SPEC §8); not now.
- [ ] **CLI-16 [P2][M] Desktop voice latency: what is left.** The
  2026-10-08 review: native WASAPI cost nothing (event-driven, MMCSS,
  20 ms render buffer). Our code adds ~74 ms one way; the estimate
  mouth to ear is 135–175 ms. First: measure (a clap test, and
  buffer depths in the voice summary). Then LAT-1 (late packets push
  playback 20 ms
  later) and LAT-3 (the 32 KiB datagram send buffer holds seconds, not
  ~200 ms). LAT-2 (share audio waited for the video tick) is done
  (§7, 1.2.1). Also seen:
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
  fires on `release: published`, which includes pre-releases, and the
  R2 `version` file moves too (the Docker image no longer moves
  `latest` for one, §7). Guard on `github.event.release.prerelease`.

## 2a. The 2026-10-10 review

A staff-engineer review of the whole repository at `0516add`
([baucord — staff engineering review](https://claude.ai/code/artifact/8a013ff5-90c8-4a37-8e1f-fe9055a2aa2e)):
7 P0, 22 P1, 62 P2 and 52 P3 items, a wakeup audit and a dead-code list,
each with file:line evidence and a fix. Its ids (`P0-1` … `P3-52`, no
clash with this file's) are the work list for 1.2.1; a finished one gets
a line in §7 naming its id, an open one stays in the review. Where a
review item finished an item of this file, that item left its list.

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
- [ ] **REL-1 [P0][S–M] A CI gate.** *In part, 2026-10-12:*
  `pr-check.yaml` is enabled again and rewritten — nightly
  `fmt --check`, `cargo check --locked` of the desktop build and the
  phones' shared code, clippy, and the tests of the eight crates that
  need no display (the versions test among them), in a Debian 13
  container, on pull requests and on pushes to `main`. **Never run on
  GitHub**: its first run is its test — the six socket-wait tests (§8)
  have only ever failed in the sandbox it was written in. Left:
  `release.yml` still runs `cargo build` without `--locked`; the release
  guard checks only `crates/desktop` against the tag; clippy does not
  yet deny warnings (the crates carry pedantic ones); the release jobs
  do not `needs:` the check.
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
  answer, AEC-SPEC §9) or Linux; macOS encodes with x264 on the CPU.
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
  corruption. (The chat pane's scrolling has its harness,
  `tools/chat-scroll-sim`.) **Done of it, 2026-10-12:** `audio`
  device supervision against a stand-in machine — `audio/src/fake.rs`
  and the 51 scenarios of `supervision_tests.rs`, each run under the
  rules of the desktop, Windows, Android or iOS (review P2-55).
- [ ] **TST-3 [P2][M] A load scenario in the simulator**: 200 clients,
  5 sharers, mixed links; assert voice loss and RTT spread.
- [ ] **TST-4 [P1] Device passes the sandbox cannot do**, before each
  phone release: Android 10, 11, 14 and 16; an older and a current
  iPhone; Bluetooth headsets; a phone call mid-session; Wi-Fi ↔
  cellular; screen off for 10 minutes; the notifications. On
  the desktop: watched keybinds per OS, the single-instance guard, the
  tray, `server restore` on a real backup, an image over 10 MiB.
  **For 1.2.1 the list is `DEVICE-CHECKLIST.md`** (2026-10-12): every
  change of the review work that was not run on its platform, per
  platform.

## 6. Decisions only you can take

1. **Licences** (LEG-1, LEG-2) — *decided 2026-10-09*: baucord stays
   Apache-2.0; a desktop build as a whole is GPL-3.0 (FFmpeg and x264);
   Slint under its royalty-free licence.
2. **Stores** — *Google Play decided 2026-10-07*: yes, PLAY-STORE.md
   (its §2 holds the decisions it needs). The App Store: still open,
   and it comes with two questions of its own. Push (4, below). And
   the lobby's keep-alive: iOS keeps a connected app alive in the
   background with a silent audio output (`audio/src/lib.rs`, the
   `audio` background mode in `ios/project.yml`; IOS-SPEC D47), which
   App Review knows and rejects under guideline 2.5.4 when no audio
   the user can hear is playing (review P2-29). In a room that mode is
   honest; in the lobby it is not. For the App Store the lobby either
   drops the connection in the background (and PRD-1's push brings the
   user back), or the app is offered outside the store only.
3. **Privacy** (LEG-5): what the official server keeps and for how
   long, telemetry consent, who answers for self-hosted servers.
4. **Push** (PRD-1): run an official push relay, or live with
   "notifications only while connected".
5. **The lobby** (MOB-1) — *decided 2026-10-07*: no microphone in the
   lobby or while muted, on the phones only; the desktop keeps its
   streams, and since 1.2.1 drops what the microphone hears there
   (review P1-6). The AFK room counts as out of a room.
6. **Mute across reconnects and restarts** (CLI-5): assumed yes.
7. **The default branch**: `main` — *taken as decided 2026-10-12*:
   it is what the repository's remote names (`origin/HEAD`), and
   `chunk` no longer exists there. `release.yml` warms its caches from
   it now. Say so if it should be another.
8. **A kick or a ban with the window in the tray** — *decided
   2026-10-12*: the window stays where it is (what review P2-17 left
   it doing).
9. **The sharer's mouse pointer** (review P2-25) — *decided
   2026-10-12*: built, on, and to be looked at on Windows before the
   release (§7; `DEVICE-CHECKLIST.md` §1.3). `"pointer": false` under
   `share` in `settings.json` is the way back for a machine where it
   draws wrong; there is no switch in the app.
10. **Chat pictures' bytes on disk** (2026-10-14, open). Decoded
   pictures are now kept only for what is on screen (§7), but each
   picture's wire bytes stay in memory for as long as its row (0.2–2
   MB, up to 10 MB a GIF): General's 500 rows of a picture-heavy
   evening can still hold tens of MB. Spilling the older ones to a
   cache folder (emptied at start, like the avatars' cache beside it)
   would bound that — but DM pictures would then sit on disk,
   unencrypted, until the next start. Assumed no until you say.

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
- **2026-10-09 — the server image**: no image had been published since
  1.1.7. The Dockerfile did not copy `third_party/`, where the
  workspace's `[patch.crates-io]` points since 1.1.8 (the vendored
  `wasapi`), so `cargo chef cook` failed on every release; it copies it
  now. Also in `docker.yaml`: *Run workflow* with a tag rebuilds a
  release's image with the current Dockerfile (it used to skip every
  manual run), a pre-release leaves `latest` alone, the tag must match
  the server crate's version; and the Dockerfile no longer fetches its
  syntax frontend from Docker Hub.
- **2026-10-09 — 1.2.0, the rest**: `Authors` (a lobby poster's name,
  offline authors' looks), the chat's stickiness by input with an
  "N new messages" pill (ANDROID-SPEC D66), General's Activity view
  (D65).

- **2026-10-10 — 1.2.1, the review's P0s** (§2a): control messages are
  17 KiB both ways, as every shipped peer already enforced — history
  pages, the DM list and admin answers are cut to fit one message
  (`protocol::keep_prefix_that_fits` and friends) instead of vanishing
  unread at the client (P0-1); refusing an upload names the upload
  restriction, not General's text (P0-2); deleting a role revokes its
  invites and opens its channels in one transaction (P0-3); the prune
  tells the tick which files it deleted, and someone else's file is
  sent once in full before it can be posted again (P0-4); the release
  builds only the desktop and the server (`default-members`, P0-5);
  teardown stops watching every stream (P0-6); links open only when
  they are `http`/`https`, on every host, and the linkifier stops a URL
  at `<`, `>` and `\` — CLI-4 (P0-7).
- **2026-10-10 — "Install and restart" relaunches.** The new process
  knocked on the single-instance socket of the one still exiting,
  heard "already running" and quit, so the app closed and never came
  back. The updater now passes `BAUCORD_RELAUNCH_AFTER_UPDATE` (and the
  same arguments, so a `--profile` comes back on its profile) and the
  child waits for the old instance to leave (`instance::wait_for_previous`).
  The socket name is now an FNV hash of the config path instead of
  std's `DefaultHasher`.
- **2026-10-10 — 1.2.1, the review's P1s** (§2a), mostly wakes nobody
  needed:
  - *Loops sleep until there is work.* Both network loops wait on the
    socket until `Driver::next_deadline()`; the server's idle main
    thread went from 50–1,000 wakes a second to a few (P1-2), and
    Windows has the socket wake too (`WSAPoll`, P1-1). Voice packets
    skip the voice thread, which wakes once a second (P1-5). The audio
    workers wait on their device latch; a stream's sound is read in its
    device callback (P1-4, which also closes CLI-14). The screen thread
    sleeps until a playout slot and no longer wakes the UI; a stream
    nobody can see is not decoded (P1-7, P1-8). Keybinds are watched
    only while connected, and only the bound keys — `device_query` is
    gone (P1-3). The roster is rebuilt once per pass and only on screen
    (P1-9); the phones' main screen is hidden under their pages (P1-10).
  - *Work nobody hears.* The desktop drops the microphone outside a
    voice room (lobby, AFK) instead of encoding what the relay throws
    away (P1-6).
  - *Latency.* A gap in a stream's frames is acted on at once (P1-12);
    NVENC and QSV open without a frame queue and a still screen's last
    change leaves at once (P1-13); capture and decode reuse pooled
    buffers (P1-11); share audio leaves as each packet is encoded
    (LAT-2) and the desktop holds a 60 ms cushion for it.
  - *Bugs.* GSO turning off no longer loses every batch (P1-14); the
    UI and the network thread can no longer deadlock (P1-16); two
    uploads at once keep separate state (P1-17); Windows reopens both
    sides when the output changes, and "Default" means one device
    (P1-18, AEC-SPEC E13); stream sound plays on the voice output
    picked in Settings (P1-19); pictures from the wire decode only as
    PNG, JPEG or GIF (P1-20).
  - *Server and scripts.* A retired key vouches for two years from its
    retirement, by the date in its name (P1-15); `monitor-net.sh`
    writes its rows again (P1-21); CLI listings change nothing, and a
    reload sends only what changed (P1-22).
- **2026-10-11 — 1.2.1, the review continued** (§2a). What the P1s
  had left open, and the first P2s and P3s:
  - *P1s finished.* Commands and state travel on unbounded channels
    both ways; our share's frames and sound go out on their own bounded
    `MediaOut` channel, so video can no longer queue in front of a
    command (P1-16). `UploadProgress` names its upload and each card
    moves its own ring (P1-17). A policy snapshot read before a write
    the tick queued afterwards is set aside and asked for again, so a
    ban is never dropped from memory by a reload (P1-22).
  - *Server.* Lobby sessions keep their speaking, audio state, renames
    and role changes to themselves (`audience_about`, P2-1). Voice the
    relay drops on purpose is still counted, so leaving a mute or the
    lobby no longer reads as loss (P2-2). A reload the next start would
    refuse is not applied (P2-4). Close frames leave before the
    database drain, which has one 6 s deadline (P2-5). Compose mounts
    the `config/` folder and names the file with `BAUCORD_CONFIG`
    (P2-6). Failed admissions are pruned and counted per IPv6 /64
    (P2-7). A CLI purge takes the messages off the live server (P2-8).
    A read that timed out in the queue is not run later (P2-9). A key
    or an address can only be banned, with `BAN`, and the fallout
    spares administrators and equal ranks at admission too (P2-10).
    The blob cache is bounded and proofs go with their blob (P2-12).
    The persistence thread sleeps until its next job and is not
    started without a database (P2-47).
  - *Server polish.* `link:` lines cover their whole interval (P3-7);
    a failed reopen after log rotation keeps writing to `.1` (P3-8);
    audit targets are compared as bytes (P3-10); `server key unlink`
    drops the live session (P3-11); message ids come from a counter
    that survives a prune, with indexes on `messages(user_id)` and
    `conversations(user_b)` — migration 5 (P3-12, part of P3-9).
  - *Client.* Slash commands split on any whitespace and one
    `protocol::parse_hex_id` serves client and server (P2-15). Transfer
    state ends with the session (P2-13). A failed retry keeps the room
    to rejoin, and a refused one releases the host's keep-alive
    (P2-14). The voice telemetry's "exercised" is measured on the
    right clock (P2-19); a failed microphone reopen clears the speaking
    flag (P2-20); a page loaded with "Load earlier" is kept while it is
    read, and paging continues from the oldest row shown (P2-23).
  - *Weight.* `zip` and `nnnoiseless` without their default features
    (P2-46); dependencies carry line tables only in dev, and the unused
    `debug-opt` profile is gone (P3-51).
  - *Still open of P0 and P1* (at the time; both done in the seventh
    batch, below): paging for the admin lists (P0-1, needs a feature
    bit) and the keybind watcher's event sources (P1-3 step 2, one
    test pass per OS).
- **2026-10-12 — 1.2.1, the review's second batch** (§2a).
  - *Protocol.* `lane()` names every variant, and so does the new
    `ServerMessage::required_feature()`; one codec serves both
    directions (P3-1). `protocol/tests/wire_golden.rs` pins every wire
    enum's variant order and every message's and struct's layout
    (P2-62). The video reassembler holds one in-order frame per sender
    (P2-56).
  - *Transport.* Sends queue and each peer is drained once per pass;
    `handle_timeouts` touches only peers that are due; `readable()`
    names the peers with something to read (P2-31). `Shared` payloads
    are put on the wire once and handed to quinn by reference (P2-32).
    The receive path sizes its buffers by the platform's batch, builds
    its slices on the stack and splits one buffer per GRO batch (P3-2).
    `ServerIdentity` and `rcgen` are behind a `server` feature (P3-3).
    The client keep-alive is 5 s (10 s at first; 5 s since
    2026-10-12), held through 40 % loss in the simulator, and a new
    bit, `QUIET_REPORTS`, stops the two-second `VoiceReceiverReport`
    to sessions that send no voice (P2-49); an idle client on a dead
    link now notices after up to 35 s, not 32.
  - *Server.* Stream health is per sharer and spans five reports
    (P2-3). File references: recounted from the rows at startup,
    released with the messages a retention pass removes (which also
    leave the replay ring), not released twice for a deleted post at a
    purge, given back when a file DM is not stored — migration 6 adds
    the index (P2-11). The transport keeps a waiting room for
    connections that have not said Hello — its own seats, eight per
    address, a QUIC Retry once half are taken — so `ServerFull` reaches
    the client and a returning user replaces their own session on a
    full server (P3-5; SEC-2 in part, below). Feature gates are one
    table checked in `send_msg` (P3-6). A first visit and every admin
    command are one transaction, and the hot statements are cached
    (P3-9). A purged account's uploaded logs are deleted off the tick
    (P3-13). Reads in flight are one `Waiting` table and their answers
    one channel (P2-52).
  - *Client.* Image and file reassembly is one type that hashes as it
    goes, and stalled transfers are dropped on the two-second tick
    (P3-14). Keyframe requests, decode errors and frames dropped before
    the screen thread are counts on the 5 s summary lines (P3-15); the
    share loop no longer allocates a preset name per tick or logs a
    line per frame (P3-16). `settings.json` is saved through a rename
    with a `.bak`, a corrupt file is set aside and never replaced by an
    older install's, and the pre-0.3 shadow struct and the unused room
    password are gone (P3-17, P3-18; CLI-6 but for showing the error
    in the UI). Volume and mute per person are filed under the user id
    (P2-24). The UI thread is woken once for however many messages are
    queued, and not at all for link stats while the app is off screen
    (P2-48). Two conditions that were always true are gone from
    `on_disconnected` (P3-20). Comfort noise is not decoded for a peer
    whose last frame was silence (P2-34). A heading with spaces around
    its text renders bold (P3-39).
  - *Build.* The product version, the edition and the licence live
    under `[workspace.package]`; the release workflows, Gradle and the
    iOS scripts read the version there (P2-61).
  - *Dead code and dead files* (the review's list, 2026-10-12). Files:
    `skills/` (69 files), `scripts/baucord_thread_audit.ps1`, six of
    the seven example programs (the audio `latency` one stays).
    Protocol, transport, server: `Carrier` and `Lane::carrier`,
    `Driver::is_server` / `is_connecting` / `has_transmits`,
    `frame::Decoder::buffered`, the reassembler's statistics, the
    relay's `group_open`, the on-connect stream announcement that could
    never fire, two one-line wrappers in `policy.rs`, `SERVER_1_0`.
    Client: the seven media-only messages are an internal enum, so the
    app's `NetworkMessage` has no arm that never runs (the first half
    of P2-53); four fields of `SharedScreenState`, two re-exports, two
    test-only `ServerEntry` helpers. Audio: the endpoint-loopback
    fallback and the macOS and Linux stand-ins of system-audio capture
    (Windows process loopback is the one backend; an older Windows 10
    shares without sound), its unused mute and volume, the microphone
    level meter nothing read (computed per sample in every capture
    callback), six setters and five getters of `VoiceChat`, three
    stats; late packets and device restarts are now on the stats line.
    Video: the `return_buffer` and `quality` trait methods, three size
    helpers, the stub decoder and the stub capturer. App: unreachable
    admin-result arms, a duplicated set of sharer ids, `ureq` out of
    the phone builds. UI: 33 messages in each of the three catalogs, 12
    `Tr` properties, 11 `State` properties, two callbacks, three struct
    fields, five functions — and `i18n/tests/catalog.rs` now fails on a
    message nothing shows or a language missing one. Android: the
    `ndk-context` and `requestPermissions` fallbacks,
    `ACCESS_NETWORK_STATE`, two Gradle flags. *Left on purpose:* the
    stored telemetry blob with its two decoders and the six unused
    database columns (dropping them deletes stored data: a migration to
    decide on), `Config.authentication` (until OPS-1),
    `DecodedFrame.timestamp_ms` (its writers include the iOS decoder,
    which cannot be compiled here), and the two `.disabled` workflows
    (to fix and enable, not to delete).
- **2026-10-12 — 1.2.1, the review's third batch** (§2a): what costs
  while nothing happens, and the audio leftovers.
  - *Timers.* The share's stats timer, the update's progress poll, the
    device poll and the resource sampler are fields that stop with what
    they serve; the list nobody could stop is gone (P2-38). The GIF
    ticker stops while the window is in the tray, minimized or a phone
    app in the background, and restarts when it is back; only the two
    newest GIFs of a page autoplay (P2-39). The Settings timer runs at
    8 Hz on the Audio tab only (2 s elsewhere and out of sight), and
    the meters no longer animate their width (P2-40).
  - *Sharing.* A still picture's once-a-second refresh stays out of
    the encoder's keyframe cadence (`encode_repeat`: a still share sent
    an IDR every 2 s, and on AMF reopened its session every 4); a
    screenshot identical to the last is not encoded (macOS, Linux, the
    GDI fallback); the GDI liveness probe runs every 30 s on a still
    screen, every 2 s only while it is the picture (P2-41). Our own
    share and the streams we watch run on a thread each behind one
    routed sender, `ScreenTx` (P2-44, out of PRD-5). `video::pool`
    takes the best fit and is freed when nothing is shared or watched
    (P2-43; VideoToolbox's per-frame zero-fill is left, iOS).
  - *Audio.* The callbacks wake their workers through `audio::latch`
    (an atomic and an unpark) and no longer through a channel that
    locks while the worker sleeps (P2-33). RED sends and receives
    without a list per packet and does not copy a redundant block it
    already holds; `adapt()` reuses its list; the peer-settings lock is
    not held across the decodes (P3-21). The stats line counts
    clock-drift windows and how many switched the correction on, to
    decide after a release whether it stays (P3-22). Out of the voice
    rooms the encoder thread looks every 5 s, not every 250 ms (P3-23).
    `VoiceChat::new` cannot fail and `start` returns a `StartOutcome`
    (P3-24). A plan check lists Android's call devices once and asks
    nothing out of a room (P3-25). The stream output's callback thread
    is promoted (P3-26). WASAPI device loss is matched on the error
    code (P3-27; the loops still do not share `pump.rs` — real-time
    Windows code, not changed unrun).
  - *Icons.* The window and the tray load a 256 px icon (6 MiB decoded
    twice before); the `.ico` and the website's favicon are 106 KB, were
    727 (`scripts/desktop-icons.py`, P2-45).
  - *Windows ducking* (CLI-11, AEC-SPEC E14): see there.
  - *Windows.* A stream whose device stopped signalling (a headset
    unplugged) asks the engine on the next event timeout and reopens
    (P2-21). A share's system-audio capture that gave up is started
    again, and a capture that restarted is no longer four billion
    skipped frames in the viewer's summary (P2-22). DXGI capture opens
    on the adapter that drives the monitor, not always the default one
    (P2-26), and with two frames queued hands out the newer when the
    screen is faster than the share (P3-28). The 1 ms system timer is
    held in a voice room and while a stream is shared or watched, not
    for the whole connection (P2-35). The capture pump no longer writes
    a log line per discontinuity, and the `wasapi` crate is capped at
    info (P2-37). **All of it type-checked for Windows, none of it
    run.** Left: the mouse pointer in a share (P2-25) — it has to be
    looked at on a Windows screen.
  - *CI.* The release build warms its caches from `main` (it named a
    branch that is gone, so no release ever restored one), installs
    the toolchain `rust-toolchain.toml` pins and no other, and no
    longer wraps rustc in sccache and its Windows shim (P2-50, part of
    P3-48); `pr-check.yaml` is back (REL-1) and **ran green on GitHub
    on `0b8c374`** (the user, 2026-10-12) — with the six socket-wait
    tests the sandbox cannot pass, so those are the sandbox's. The
    release workflow's changes have not run yet. Left: one release
    workflow instead of four (P2-60), the package lists and `cargo-ndk`
    (P3-48) — each needs a release run to prove.
- **2026-10-12 — 1.2.1, the review's fourth batch** (§2a): what the
  user sees. From here on Slint changes are *looked at* in the sandbox
  (guide §1, "The loop": the aarch64 viewer), not only compiled.
  - *Windows 10 ducking, decided* (CLI-11, AEC-SPEC E15): keep the
    canceller, say what Windows is doing, and put the way out one
    click away.
  - *Bugs.* A connect, a reconnect or a lost connection no longer
    brings the window forward or takes the focus; it is shown at
    launch (unless that starts in the tray) and when the user asks
    (P2-17). The composer menu's Cut and Copy work: the selection
    travels with the right-click and the edit is done host-side, and
    they are greyed without one (P2-16); a callback that re-enters the
    app state is dropped with a log line in a release build instead of
    aborting it. Pasting a picture works on Linux and from
    bitmap-only sources (PrintScreen, a paint program) elsewhere:
    with no text on the clipboard, its pixels are taken through
    arboard (P2-18). A saved picture is named for what its bytes are,
    not always `.png`; a phone's menu has no "Copy image" (P3-31).
    A device plugged in with Settings open no longer shifts what a
    pick means, and the pickers refresh as devices come and go
    (P3-35's second half). The profile card waits its dwell again on
    the same row and stays away under a menu (P3-37). The connect and
    startup cards fit a 360–375 pt phone, the terms line wrapping onto
    a second line there (P3-40). Share denied, can't watch, upload
    refused, channel join refused and the time units ("3 d", "5 min
    ago") are in the three catalogs (P3-30; an admin's command output
    — flags of `/channel list`, the restriction list — is still
    English).
  - *A GIF's loop length was misread* — the delay one byte early, so
    a 40 ms frame counted as ten seconds and nearly every GIF got the
    30 s autoplay cap instead of "about three loops". Found by a test
    written for P2-59. **GIFs now freeze after ~3 loops**, as
    `gif.rs` always said they did; hovering replays them as before.
  - *Simpler.* One GIF player: the gallery and the send preview loop
    the streaming player; the up-front decoder (240 frames, 48 MB) is
    gone (P2-59). A menu action is described once (P2-57). The emoji
    palette builds only the rows in view (P3-38). One bounded-encode
    loop, no copy of the picture before it, no second full decode of a
    pass-through picture to read its size (P3-41, P3-42). The phones'
    hooks lose `notifies` (both said yes) and a repeated
    `target_exists`; the chime's WAV is borrowed, not cloned (P3-47 —
    **the two-line deletions in `crates/android` and `crates/ios` are
    not compiled here**).
  - Left of the UI list then: see the fifth batch.
- **2026-10-12 — 1.2.1, the review's fifth batch** (§2a): the UI
  list's simplifications. Checked with a scratch harness that drives
  the real `MainWindow` headless (Slint's software renderer, real key
  and pointer events) against the tree before and after: 131 observed
  facts and 51 screenshots, identical.
  - *One dialog, one row frame, one ring* (P2-58). `Modal` in
    `widgets.slint` is the scrim, the centred card, the click outside,
    Escape and Android's Back; eight dialogs in `main.slint` are its
    content (`Theme.scrim`). `RowFrame` in `chat.slint` is a person's
    row: hover tint, right-click and long-press menu, gutter picture,
    name line; the text, code, picture and file rows inherit it.
    `ProgressRing` is the two copies of the transfer ring.
    `main.slint` 1345 → 1210 lines, `chat.slint` 1801 → 1661. The
    harness caught the one thing a render cannot: a field that
    focused itself in its own `init` lost the keyboard to the shared
    dialog, so typing and Enter went nowhere in three dialogs — they
    now take it in `Modal`'s `opened`.
  - *One host module* (P3-33): `frontend/src/host.rs` for every host;
    the desktop's answers are its `DesktopHooks`, a phone installs its
    own. `stubs/host.rs` is gone. The `Hooks` trait asks nothing new
    of the phones (three defaulted methods were added).
  - *The session is one value* (P2-51, the frontend half). The 53
    fields that belong to a connection moved from `AppState` into
    `Session` (with `Safety`, `Transfers`, `ChatFlow` inside it);
    `teardown_connection` replaces it whole, where three hand-kept
    lists reset it a line per field. **Not done: the server's half.**
    Its teardown is already one function, and the relays read several
    of its per-session maps on every packet; folding them into one
    struct changes hot-path signatures for little gain — to decide.
  - *Smaller.* A decoded frame's figures travel as one `FrameMeta`
    (P3-36). The admin panel keeps the `AdminCommand` it will send,
    not a second enum describing it (P3-34). Settings' thirteen plain
    switches are one table that `populate` and `wire` both expand
    (P3-35).
  - Left of the list then: one audio `reconcile()` (P2-55); see the
    next entry.
- **2026-10-12 — 1.2.1, the audio engine's device handling in one
  place** (review P2-55, and TST-2's audio part), in two steps so the
  second could be held to the first.
  - *A machine that is not there.* The rules that differ by platform
    (`PLATFORM_VOICE_PATH`, `RELEASES_MIC`, `SHARED_VOICE_UNIT`, and
    what were `cfg` branches for Android's routes and Windows'
    reference and two defaults) are a `Traits` value the engine
    carries — the build's, or in a test any platform's. The platform
    is asked through four small functions (`survey`,
    `engage_call_path`, `open_playback`, `open_capture`) which
    `audio/src/fake.rs` answers in tests, with sides that hold no
    stream and a log of every plan, open and close. 43 scenarios were
    written against the engine *as it was* (42 as predicted from
    reading it; the one surprise was the reading), then eight more.
  - *One `reconcile`.* `start`, the presence, the two device picks,
    the setting and `service` each decided what to close and open,
    with their own copies of doing it. Now each records what it was
    told and calls `VoiceChat::reconcile(cause)`: `decide` holds every
    rule side by side and returns keep / re-plan both / these sides;
    `plan_and_open`, `apply_sides`, `open_output` and `open_input`
    are the only places a stream is opened or closed. All 43 traces
    came out identical on the first run, and the old engine still
    passes 50 of the final 51.
  - *The one difference, on purpose:* a microphone pick that fails to
    open now clears the echo-cancellation report (Settings showed "the
    canceller is running" for a microphone that was closed); every
    other failed reopen already did.
  - **Not run on any real device.** The Windows- and Android-only
    lines type-check; the eight iOS-only blocks were compared with
    the old ones text for text (identical) but are not compiled. What
    the tests hold is the *decisions*; the streams themselves
    (`input.rs`, `output.rs`, `native/`) were not touched.

- **2026-10-12 — 1.2.1, the review's sixth batch** (§2a): what could
  still be done in the sandbox, and `DEVICE-CHECKLIST.md` for what
  could not.
  - *Smaller.* `NetworkControl::Send(ClientMessage)` replaces the 17
    variants that restated a wire message field for field, and their
    17 arms (`network.rs` −201 lines; the rest of P2-53). The
    encoder's twelve methods live in its `VideoEncoder`
    implementation, without an inherent copy each and a forwarder
    (P3-29). The microphone's chain is a struct with nine fields, not
    a `Vec<Box<dyn AudioProcessor>>` built by `add` calls, and the
    stages' unused names are gone (P2-54).
  - *One of each.* SHA-256 is `transport::sha256` on `ring`; `sha2`
    and what it brought are out of the tree (standard vectors tested;
    stored hashes and pinned fingerprints are the same bytes). One
    `pollster`. winit through `slint/unstable-winit-030`, not Slint's
    internal backend crate (P3-50). `chrono`, `serde_json`,
    `env_logger` and the `windows` version are
    `[workspace.dependencies]`; Slint is `~1.18.1` (P3-49). *Left:*
    `sysinfo` on the phones (the resource line and `/mem` use it
    there), and `audio`/`video` on edition 2021 — the migration has to
    be compiled for iOS and macOS, which the sandbox cannot.
  - *Words.* The send preview is decoded at the size it is drawn
    (P3-41). `/channel list`'s flags, the admin lists' "no roles",
    "key …", "uses …", "(revoked)", and the nine fixed reasons an
    admin command is refused are in the three catalogs (P3-30). Left
    in English on purpose: the restriction kinds (`voice`, `text`, …),
    which are what an admin types, and a reason the server wrote
    itself.
  - *Tests.* `tools/chat-scroll-sim` covers the row cap: appends at
    500 rows at the bottom, the one trim above a reader who has just
    scrolled up, the held list to 2,000 rows and past it, and 1,502
    rows leaving at once after the jump back — the view never moves
    (P3-43; no bug found). The transport simulator runs the four
    things it never had: a session the server forgot is reset at the
    client's next packet, a refused stream (STOP_SENDING) costs one
    group, voice into a stalled link is dropped, counted and reported
    once, two tracks over a reordering link each arrive whole and in
    order (P3-4). `a_report_reaches_the_moderators…` no longer waits
    500 ms of *simulated* time for a write the database thread does in
    real time (it failed about one run in five on a busy disk).
  - *The host script.* `setup.sh` no longer sets `net.core.*_default`
    (every socket on the box; the server sizes its own) or mentions
    0.5's `netcode.key`. The operator's guide is `web/deploy.html`
    alone — the readme's copy is a pointer, and the two details it
    had over the page moved there (P3-52). *Left:* the cloud-init
    hotplug switch, which is a decision about the host, not a trim.
- **2026-10-12 — 1.2.1, the review's seventh batch** (§2a): what was
  left, as far as it can go without the machines. `DEVICE-CHECKLIST.md`
  marks its lines **[7]** and has a macOS section now.
  - *Admin lists of any length* (P0-1, the rest). Feature bit
    `ADMIN_PAGES` (19) and one new message, `AdminRows`: a listing
    longer than one 17 KiB message goes out as pages — the rows ahead
    as `AdminRows`, then the `AdminResult` that closes the answer —
    and the client joins them before the app sees one list
    (`network::AdminPages`). A session without the bit gets the first
    page, as before. At most 64 messages an answer. Reports go through
    the same path (200 of them at their longest are 34 messages; they
    were cut to six). A typed `/users`, `/bans`, `/invites` or
    `/reports` prints 150 rows and says how many more the panel has.
  - *Keybinds without a clock* (P1-3 step 2, P2-30). The watcher is a
    crate of its own, `keywatch`: it sleeps until a key a binding
    names changes — Windows Raw Input, a listen-only event tap on
    macOS, XInput2 raw events on X11 — and reads the key state then,
    a few times more, and every 12 ms only while a binding is held.
    Measured on X11 (a virtual display, keys from `xdotool`): 0 wakes
    with the keyboard at rest, where it was about 166 a second; one
    per event for keys nobody bound. The events are trusted only once
    three presses have arrived with one; until then a 250 ms heartbeat
    also reads, and a press it finds that no event announced sends the
    watcher back to the 12 ms clock for good, with a log line.
    **The Windows and macOS halves were type-checked and linted for
    their targets and never run.** On macOS the Accessibility prompt
    comes only from a button in Settings → Keyboard, which shows only
    while the permission is missing.
  - *The phones' shared parts* (P3-44, P3-45, P3-50). One log writer
    for both (`app_core::logfile::FileSink`), which rolls the log over
    at 5 MB as it is written — it was at launch only; one panic hook
    for the desktop and both phones; one safe-file-name and
    never-overwrite pair (the desktop's Downloads uses it too) and one
    pick slot (`app_core::files`). A phone build no longer carries
    `sysinfo`: its CPU and memory figures are `getrusage` and
    `/proc/self/statm` or `task_info` (`app_core::usage`, tested on
    Linux and type-checked for both phones). iOS's memory figure is
    the footprint now.
  - *Android, unbuilt* (P2-27, P2-28, P3-46). A saved file is written
    by Rust to a descriptor `MediaStore` hands over, on a thread of
    its own (every platform's save left the UI thread) — it crossed
    into the Java heap whole. The room's wake lock lasts 10 minutes a
    time and is taken again when a reconnect starts. A release build
    is arm64-v8a unless told otherwise (`gradle.properties` said both
    ABIs for every build). The host's Rust was type-checked in a
    scratch crate and the Java compiles against API 34's
    `android.jar`; **Gradle has not built it and no phone has run it —
    the wake lock is the line to check first.**
  - *Video on Apple hardware* (P2-42, P2-43). `h264_videotoolbox` is
    tried before libx264 (hardware only, `realtime`); a Mac that
    cannot open it gets libx264 as before. iOS's decoder hands its
    picture on as BGRA (`PictureFormat::Bgra`) and the display swaps
    two bytes a pixel as it writes its own buffer: one pass and no
    cleared buffer per frame, where there were two and one. The iOS
    half is type-checked for iOS; neither was run.
  - *Edition 2024 for `audio` and `video`* (P3-49, the rest).
    Type-checked on Linux, for Windows and Android, and `video` for
    iOS; `audio`'s iOS code was read for what the edition rejects.
    Sixteen nested `if`s became let-chains (clippy denies them there).
  - *The workflows* (P2-60 and P3-48, in part). The release checks its
    tag against the workspace version in a first job, not the last;
    the `version` file goes to R2 in a step of its own after the zips.
    The Linux package lists lost what no crate links (`libopus`,
    `libdbus`, and for the release Xinerama, Xft, Xrender, pango,
    cairo); `cargo-ndk` is a pinned prebuilt. All four workflows pass
    `actionlint`; none has run.
  - *The host script* (P3-52, the rest). The cloud-init hotplug switch
    is written only where there is a cloud-init, said in the script's
    output, and left alone (or taken back) with
    `KEEP_CLOUD_INIT_HOTPLUG=1`.
  - *Not done, and why.* The pointer in a share (P2-25; §6, 9). The
    WASAPI loops still do not share `pump.rs` (P3-27): a refactor of
    real-time Windows code with nothing to gain but lines, not done
    unrun. One release workflow in place of four (P2-60): merging
    them makes one platform's failure everyone's — the website's
    deploy waits for the release workflow to succeed — so it wants a
    decision on that and a dry run, not a guess. The server half of
    P2-51 (decided against: its teardown is one function already).
    The migration that drops unused telemetry columns (it deletes
    data). App Store questions (§6, 2).
- **2026-10-12 — 1.2.1, after the first iPhone look.** Two things
  the phone showed, both the same on every platform.
  - *Settings → Audio opens the microphone.* The capture bar was dead
    unless in a room (and on the desktop unless connected), so there
    was no setting a microphone up before a call. While the tab is on
    screen the microphone is open: not connected, on an engine with
    no session, whose packets are never sent and which mute and
    push-to-talk do not close (`VoiceControl::Start` without a client
    id, `VoiceLink::local_only`); in the lobby or the AFK room, on the
    session's engine with the packets and the speaking light kept
    here. It ends with the tab, with Settings, and with a phone app
    leaving the screen. A microphone test not connected with
    push-to-talk chosen was silent for the same reason, and is not
    now. On Windows 10 the tab's streams are a room's, so other apps
    may get quieter while it shows.
  - *The meters are eased.* 20 readings a second while the Audio tab
    shows, a rise taken at once and a fall over 300 ms
    (`app_core::meter`, 6 tests), a bar set only when it moved, and
    the bar gliding to each value over 80 ms (`animate width` is back,
    now that it only draws while a bar moves on that one tab); 8
    readings a second drawn as read stepped. The first cut of this
    (30 readings, 150 ms, no glide) was reported from the phone as
    jumping, and a bar seen to step. The echo-cancellation line beside
    them is looked at 4 times a second.
  - *A phone's microphone test plays back a second late*
    (`audio::LOOPBACK_DELAY`, 4 tests on the delay line). Live through
    the loudspeaker it sounded about to feed back: the canceller was
    taking the user's voice out of the microphone while they were
    still saying it. The desktop's is live as it was.
  - *Open from the phone*: the report was also that the Test button,
    not the tab, opened the microphone. Both reach the engine through
    the same function with the same commands. A second look with this
    build gave one log line — the tab and a test both on, not
    connected, an engine already running — which rules out an old
    build and a tab that is never noticed, and does not say whether
    the bar moved before the test. Read again from the tab to the
    bar (the page's `changed` handlers, the timer's pace, the engine's
    start, the voice unit, the capture pump, the level, the network
    thread's unbounded queue): no fault found. What is left is the
    voice unit's start-up, a second or so in which the bar is dead
    and a test gets pressed — a guess until measured. The line logged
    when the tab leaves now measures it (`app_core::meter::Run`, 2
    tests): when an engine stood behind the meters, when the
    microphone was first heard with no test running, and the peaks
    with and without one. The `microphone:` lines name which engine
    they are about; the one sent back said "the session's engine" of
    an engine with no session. An engine is counted as the session's
    from `Init`, not `Connected`, so a tab opened between the two is
    not left without one.
  - *Checked*: Linux type-check and lint of all of it; the ease and
    the engine's mute rule by unit test; the client and `app-core`
    type-checked for Windows and Android. **Not run**: there is no
    audio device in the sandbox and the app's own tests do not fit on
    its disk. `DEVICE-CHECKLIST.md` marks the lines **[8]**.
- **2026-10-12 — 1.2.1, the check before a release.** Every id of the
  review (§2a) read against the code again, by four separate readers
  told to judge the code and not this file; the documents against the
  review's list of 24; and the whole tree built, linted and tested.
  - *The numbers.* After what was fixed here, 127 of the 143 ids are
    fixed as the review asked; 14 are fixed in part (below); P2-25 is
    open; P2-29 is a decision (§6, 2). 802 tests pass in the sandbox
    and the same six that need a real UDP socket do not; lint is where
    it was; Windows, Android, macOS (`keywatch`) and iOS (`video`)
    type-check.
  - *Found, and fixed here.*
    - **Two files dropped together lost the first** (P1-17, which
      this file called done). The server holds one announced upload a
      session; the client announced both at once, and the first was
      refused. The client now takes turns (`client/src/upload_turns.rs`,
      3 tests): one blob with the server, the rest waiting unannounced
      — against every server shipped. The integration test's stand-in
      server accepted every announcement, which is why it passed; it
      keeps the real rule now, and fails without the queue (run here
      with the loop forced onto its polling fallback).
    - **A ban of an equal left them in the room** (as old as 1.1). The
      ban command may name anyone short of an administrator; the rank
      rule that spares whoever else shares the key or address spared
      the person named too. They leave at once now (e2e test).
    - **A `server` restriction takes the ban permission by any
      command** (the rest of P2-10): `Restrict` asked for it only when
      the target was a key or an address (e2e test).
    - **The lobby's progress stays in the lobby** (the rest of P2-1):
      the once-a-minute `UserProgress` and a level-up's `UserDetails`
      go through `audience_about` like the rest.
    - **No Copy in the image viewer on a phone** (the rest of P3-31).
    - **The site is deployed after a release's build only**
      (`web.yaml`): since P2-50 "Build and Release" also runs on every
      push to `main`, and each of those runs deployed the site again.
    - *Documents*: the readme's edition line, START-HERE's decision
      ranges, IOS-SPEC's D57–D60 in order, three comments naming
      toolkits long gone. P2-36, P3-19 and P3-32 were done in earlier
      batches and had no line here; they are named now.
  - *In part, and left so.* By decision: P2-40 (the glide is back, the
    timer idles at 2 s off the Audio tab), P2-49 (5 s), P2-51 (the
    server half), P2-60, P3-27, P3-52 (the hotplug switch is said and
    can be kept). Not finished, and no user would see it: P2-11 (file
    references are recounted at startup, not after a prune), P2-27
    (a saved picture and an exported key are still written from the UI
    thread on Android), P2-32 (about twenty broadcasts still encode per
    recipient), P2-54 (three switches nothing turns are still read
    every frame), P2-58 (the admin confirmation is its own dialog, and
    does not hear Android's Back), P3-23 (the lobby's two threads tick
    slowly where the review asked them to block), P3-48 (macOS still
    installs opus; two `rustup` calls without `--profile minimal`),
    P3-49 (the `objc2` family is declared per crate).
  - *Seen, and not touched.* A DM store already running when its 2 s
    deadline passes is stored after the sender was told it failed (a
    second copy if they try again; it takes something holding the
    database for 2 s). A share's sound that fails to restart once
    stays off until the device changes or the share is restarted.
    `limits.max_chat_bytes` above about 16 KiB makes "Load earlier"
    ask for the same page for ever (the default is 2000). The release
    builds `-p desktop -p server` together, so the server's
    certificate code is compiled into the desktop (P3-3's saving is
    the phones'). `LATENCY-SPEC.md` was named by START-HERE and CLI-16
    and was not in the repository: old, and dropped the same day; what
    CLI-16 still means to do is in its own lines.
  - *What a release still needs — none of it is code.* (1) `pr-check`
    green on the branch: it runs the app's own tests and the six this
    sandbox cannot. (2) A build on Windows and on a Mac: no workflow
    compiles either before the release does, and the Windows and macOS
    halves of the app itself were never type-checked here. (3)
    `DEVICE-CHECKLIST.md`: 133 lines, none ticked — the voice,
    ducking and keybind sections on Windows first. (4) The dry runs
    the phone and Docker workflows have (`workflow_dispatch`). (5)
    Merge and publish in one sitting: a push to `main` that changes
    `Cargo.lock` or `web/` deploys the site with the new version's
    download links at once, and they lead nowhere until the release's
    builds have uploaded; the APK's a little longer, its workflow
    being its own (P2-60).
- **2026-10-12 — 1.2.1, the last asks before the release.** The user
  has built and run the branch on Windows, macOS, Linux and an iPhone
  ("works well"; Android and the server still to try), which is what
  the entry above said a release was waiting for.
  - *The mouse pointer in a Windows share* (P2-25, the last open id).
    Desktop Duplication gives the desktop without it and describes it
    beside each frame; `dxgi.rs` keeps the description and draws the
    pointer into the copy (`video/src/pointer.rs`: the three kinds
    Windows has — colour with alpha, masked colour, the two-mask
    monochrome one of the text caret — clipped at the edges; 6 tests,
    which run on Linux). A move with nothing else changing is a frame
    of its own, read again from the staging texture, and one that
    found the queue full is retried every 8 ms, so the pointer does
    not stop a frame behind where it stopped. The silence probe
    compares everything but the pointer's rectangle: a screenshot has
    no pointer, and compared whole it would have replaced the picture
    with one two seconds after every stop. Known edges: a share of a
    screen where nothing at all changes shows the pointer from the
    first change (the first picture is a screenshot); the screenshot
    fallback (an HDR desktop) still has none. Type-checked and linted
    for Windows; **never run**.
  - *A phone's microphone test plays back half a second late* (it was
    a second; `audio::LOOPBACK_DELAY`). The hint says so in the three
    languages.
  - *No "Done." in the chat.* A kick, a mute or a move from a menu
    printed it into whatever conversation was open. A menu pick now
    says nothing when it works (what it did shows where it was
    picked); a typed command, which has no other answer, gets a line
    in the activity log — `/unban bob: done`. Lists and refusals land
    in the chat as before.
  - *The image viewer is full screen on a phone* (and in any window
    under 600 px): no margins, the picture edge to edge under the
    title row, the two arrows on the picture's own edges and only
    where there is a step to take, clear of the notch. It was under
    two thirds of the width: 16 px each side and a 40 px arrow column
    each side. Rendered at 375 × 812 and 1100 × 700; the wide layout
    is as it was.
  - *`LATENCY-SPEC.md`* is no longer named anywhere (old, the user).
  - *Checked*: the tree builds, lints where it was and passes its
    tests on Linux; `video` and `client` type-check for Windows and
    Android. The pointer, the viewer on a real phone and the delay by
    ear are the user's to look at; `DEVICE-CHECKLIST.md` has the
    lines.

- **2026-10-14 — the second review's first two areas** (server core
  `SRV-n`, protocol and transport `PT-n`; [baucord — staff engineering
  review, October 2026](https://claude.ai/code/artifact/3679c79a-0419-46f3-b4c1-f708bff49621),
  the "Interim" section; the other twelve areas are not reviewed yet).
  - *Bugs users could hit.* A file larger than the per-minute transfer
    budget (4 MiB by default, 20 MiB on the official server) was
    refused for good, as "today's allowance": an empty minute now takes
    any one upload whole, the budget refuses as `Refused`, and it is
    checked before the store evicts anything (SRV-1, e2e test). A
    stalled file upload held up to 50 MB until its sender left: files
    are pruned like pictures, by the last chunk's time, and capped at
    the announced size (SRV-2). A block list past about 270 never
    arrived: at most 250 now, the part that fits for anyone over, and
    roles at most 100 so `RoleList` always fits (SRV-4, tests). An
    admin reason is cut to 500 bytes on both sides, and an answer
    whose last page cannot be sent still closes (SRV-5). `server
    restore` takes only plain relative blob paths (SRV-6, test). A CLI
    mute and its lift are told to the person, and a reload that changed
    the UI channels rebuilds the list (SRV-12, e2e test). The lobby
    cannot watch a share, and a stop with no share behind it is
    ignored (SRV-11, e2e test).
  - *The policy set-aside* (P1-22, SRV-8): only writes to tables
    `load_policy` reads count (`Command::changes_policy`); after four
    stale snapshots the tick keeps its policy and asks again a second
    later, three times, before taking one.
  - *Work on the tick.* Uploads are hashed as their chunks arrive, not
    whole at the end (SRV-3: 50 MB was 60–125 ms with voice waiting).
    Voice is offered to the speaker's room only (`room_members`,
    SRV-7). Twenty broadcast loops are one `send_to` each (SRV-9,
    PT-3), and a relayed picture is framed once for every receiver,
    handed to quinn by reference (PT-2; SEC-7 in part). Blocked pairs
    are found per blocked person, not per pair of sessions (SRV-10);
    the panel's user list builds its maps once (SRV-14); the waiting
    room counts in one pass (PT-5).
  - *Transport.* A handshake that never finishes leaves the waiting
    room after 10 s, not quinn's 30 s (PT-7, simulator test); a
    connection counts under the address it arrived from (PT-6). The
    client holds at most eight admin answers open (PT-12, test).
    Fit-to-size helpers count bytes without encoding (PT-14).
  - *Small.* The wall clock catches up after a pause (SRV-13, test);
    the TLS key log needs `BAUCORD_TLS_KEYLOG=1` (PT-17); `protocol`
    drops `log`; `ChatServer::fingerprint`, `AdminOutcome::fit_to`,
    `PinnedVerifier::pin` and `SimNet::addr` are gone,
    `ServerIdentity::cert_der` is test-only; the quinn 0.11.17 comment
    is now the decision it is (PT-20: drop-oldest waits for LAT-3).
  - *Not done, and why.* PT-1 (a deadline heap in the driver) and PT-8
    (a copy-free frame decoder): M each, on the hot path, and their
    gain is unmeasured — measure the server at 100+ peers first. PT-9
    needs a NIC that refuses segmentation offload to test. PT-10,
    PT-15: small allocations. PT-16 and the rest of SRV-15: test
    scenarios, not fixes. The `BlobChunk` length check (PT-13): only
    the comment was corrected, since tightening a decode check could
    refuse a shipped peer's chunks. SEC-7's per-peer media cap and
    SEC-3's coalescing of speaking edges stay where they are.
  - *Checked*: protocol, transport, server, client and frontend lint
    where they were (no new warning kinds), and the phones' build of
    `frontend` type-checks; every test of protocol (98), transport
    (65), server (74 + 105 e2e) and client (65 + 2) passes in the
    sandbox, the socket-wait ones included this time. Nothing was run
    against a real client or a real deployment: `DEVICE-CHECKLIST.md`
    §5 has the lines, marked **[10-14]**.
- **2026-10-14 — the client asks after the review** (the user's list:
  pictures and memory, tooltips, long press, the tray, a sound for
  mute).
  - *Pictures* (PRD-6 in part). A chat row held its decoded preview for
    its whole life — up to 2 MB of RGBA a picture at 2×, every picture
    in every conversation — and each one was decoded on the UI thread
    as it arrived, a history page's worth at once. Now a row holds no
    pixels (`ChatRow.image` is only a playing GIF's frame): the pane
    asks `Logic.chat-picture(row-id, image-rev)` for the rows its
    `ListView` built, which are the ones on screen; `pictures.rs`
    keeps the wire bytes per row and an LRU of previews (16 MB of
    RGBA beyond what is on screen), decodes a missing one on one
    worker thread (newest request first), and bumps the row's
    `image-rev` when it is ready; it fades in over the empty frame.
    Rows are laid out from the header's size (`received_dimensions`),
    so "Load earlier" decodes nothing. Clear chat now drops only the
    shown conversation's pictures (it dropped every DM's bytes too).
    `/mem` reports pictures, decoded previews and their bytes. The
    wire bytes themselves stay in memory (0.2–2 MB a picture, 10 MB at
    most): spilling them to disk would cut that too, but would write
    DM pictures to disk — a decision for §6, not taken here.
  - *Tooltips* are drawn by one layer per window (`Tip` global,
    `TipLayer` in `widgets.slint`, last child of each window): a
    tooltip that was a child of its button was drawn under whatever
    came later in the tree — the chat's under the sidebar, the
    sidebar's under the channel names. 400 ms to show, then quick
    while moving between buttons; kept inside the window, flipped
    above or below; off on the phones (no hover).
  - *Long press* (phones): the release of a hold is never also a tap.
    A held person shows their card and opens nothing on release; a
    held channel opens its menu instead of joining; a picture opens
    the viewer on a tap and the menu on a hold; a hold on an avatar
    in chat is the author's card, not the row's menu. `LongPress`
    moved to `widgets.slint`; every tap target inside a chat row is
    guarded by `held`.
  - *The tray*: a flat 64 px icon that reads at 16 px
    (`scripts/tray-icons.py`), red while nobody hears us (muted,
    deafened or muted by the server). Connected, the menu has Mute /
    Unmute and Deafen / Undeafen, and the voice room's people, each to
    mute or unmute for us alone; blocked people are left out. Labels
    say the state: the native menus draw no check marks.
  - *A sound for mute and deafen*: one soft note, G4 going quiet and
    B♭4 coming back (the join chime's two notes, quieter and shorter);
    Settings → Sounds → "Mute and deafen", on by default.
  - *The hover card* wraps a long name instead of cutting it: the rows
    elide it, and the card is where it is read in full.
  - *Checked*: the tooltip and long-press behaviour on the real
    `MainWindow` in a headless harness (Slint's software renderer, a
    mocked clock, touch and mouse); desktop and phone renders;
    `tools/chat-scroll-sim`; lint, the phones' type-check and tests as
    the commit says. Not run on a device: `DEVICE-CHECKLIST.md` lines
    marked **[10-14c]**.

- **2026-10-15 — Settings and the admin panel are pages; the admin
  panel on the phones** (ANDROID-SPEC D71, IOS-SPEC D61; the user's
  ask). On the desktop both were windows of their own; they now cover
  the main window as Settings already did on the phones, their tab
  list where the sidebar was, Escape stepping back as Back does (after
  a keybind being recorded or a confirmation has had it). The phones
  get the admin panel — the Admin chip, a list of its nine tabs, one
  tab as a page — with every tab laid out for 360 dp: rows stack under
  600 px of content (a desktop window that narrow too), the badge
  pickers wrap, a restriction's reason and a report's message are no
  longer cut. `SettingsWindow`, `AdminWindow` and the
  `host-single-window` toast are gone; the stream viewer stays a
  window on the desktop. Driven headless at three sizes in the three
  languages; lint and both `frontend` builds type-check; not run on a
  device or a desktop OS: `DEVICE-CHECKLIST.md` lines **[10-15]**.
- **2026-10-15 — the tray's people in the menu itself**: the voice
  room's people are items of the tray menu, not of a submenu named
  after the room (still only our room's); read back from Slint's Linux
  tray over D-Bus.
- **2026-10-15 — Settings' Close; Actions; the pages' column**
  (ANDROID-SPEC D72, IOS-SPEC D62; the user's asks). Settings' footer
  has Close beside Save and close: it puts back what the page changed,
  in the running app too, and keeps nothing (Escape and Back still
  save); on a phone Reset to defaults moved to the end of the section
  list. The admin panel's Users tab has one "Actions ▾" per person (the
  app's menu: Disconnect, Kick, Ban, Reset profile, Purge, each still
  confirmed) instead of five buttons. On a wide window both pages keep
  their content and buttons in one column beside the tabs. Reset to
  defaults now also re-applies the echo canceller's mode, the log level
  and the name's colour. `DEVICE-CHECKLIST.md` **[10-15]**.
- **2026-10-15 — no squares for symbols on Windows and Android**: the
  dropdowns' "▾" (a square on both) is a Lucide chevron; the status
  dot is drawn and the transfer rates' "↑"/"↓" are icons; `/users`
  says "now" instead of "●"/"○", `/reports` "handled" instead of "✓",
  and the device warning drops its "⚠". The app ships no font and
  Slint does not fall back for a character Segoe UI or Roboto lacks
  (SESSION-GUIDE §4 has the rule and how to see it here).
  User-typed symbols in chat can still show as squares on a phone: a
  bundled fallback font would cover them, not done.
- **2026-10-15 — the windows' icon is the tray's glyph**: the main
  window loads `tray.png` (it shrank the painted icon, a grey blur at
  a title bar's 16 px) and the stream viewer, which had no icon, too;
  on Windows that is also their taskbar button and Alt-Tab (the
  user's choice), while the exe keeps `icon.ico`. `icon-256.png` had
  no other reader and is gone (`scripts/desktop-icons.py` no longer
  writes it; its `.ico` is byte for byte the same). Read back from a
  real X11 window: 64×64, the glyph.

## 8. Facts this plan rests on

- **Tests, 2026-10-12, Linux sandbox**: protocol 91 + the versions
  test + 3 wire-pinning tests, transport 61, server 70 unit + 99 e2e,
  client 57, app-core 120, audio 164, video 77 + 6, i18n 2 — all
  green. Six more need a real machine:
  that sandbox does not signal UDP readiness, so the socket-wait tests
  (`transport` `udp::read_waiter_tests`, 3; `client` `inbound_wake`,
  `network_idle.rs`, `network_uploads.rs`) cannot pass there. The
  frontend type-checks for the phones and the desktop. The chat
  pane's scrolling checks (`tools/chat-scroll-sim`) passed again on
  2026-10-12, after the unread UI properties were removed, and once
  more the same day after the fourth batch's `chat.slint` changes.
  Windows-only code in `transport`, `audio`, `video`, `client` and
  `server` type-checks for `x86_64-pc-windows-gnu`, and Android-only
  code in `audio`, `video` and `client` for `aarch64-linux-android`
  (SESSION-GUIDE §1; first done 2026-10-12 — the WASAPI and socket-wait
  changes of the P1s had never been compiled before). Not covered: the
  frontend's Windows and macOS code, anything iOS, the `android` crate
  beyond its JNI module, Gradle and the Java. Nothing was run on a
  device.
- **The release pipeline** (every build first refreshes the
  third-party notices, `licenses.yml`):

  | platform | trigger | build | signing | users update by |
  |---|---|---|---|---|
  | desktop (win-x64, macOS arm64/intel, linux-x64) | release published | GitHub matrix, static FFmpeg 7.1, `--profile deploy --features tray`, link checks | none | in-app updater (unsigned zip from R2) |
  | server image | release published / manual (a tag) | buildx, amd64, distroless, uid 10000 | none | `docker compose pull` |
  | Android | release published / manual | cargo-ndk + Gradle `assembleRelease bundleRelease`, arm64, 16 KB check | the release key, after the build (APK and AAB) | sideloading the APK; Play once listed |
  | iOS | release published / manual | macos-26, Xcode, xcodegen, archive | Ad Hoc profile + certificate | reinstalling the `.ipa` |
  | website | `web/**` or the notices' inputs on main, or after any build | stamps the desktop version | — | — |
