# baucord — start here

baucord is a voice, text and screen-sharing app in Rust: a self-hosted
relay server (QUIC, no media mixing), a desktop client for Windows,
macOS and Linux, and clients for Android and iPhone that run the same
Rust engine and the same Slint UI. It is in daily use: 1.2.1 is the
latest release (2026-10-13; 1.3.0, with the 2026-10 review's fixes, is
next, CHANGELOG.md), desktop users on the in-app updater, Android as a
sideloaded APK and on Play's internal test, iPhone as an Ad Hoc build,
the official server in Docker.

This file is the way in. Read it first, then follow the reading order
for the work at hand (§3). Everything else is linked from here.

## 1. Starting a session (for the assistant)

1. Read this file, then **SESSION-GUIDE.md §1** (set up the sandbox and
   the checks) and the parts of **§4** (rules learned the hard way) that
   touch the area you will work in.
2. Read **PRODUCTION-PLAN.md** §1 and the headings of §2–§5: it is the
   one list of what is left, with ids (`SEC-3`, `MOB-1`). A request is
   often one of its items; a new feature goes there too.
3. Read the area documents for the task (§3 below).
4. Say in a few lines where you will start and what you assume; ask
   only what the code and the docs cannot answer; then work.

## 2. How work moves between the user and the assistant

- **In:** the user uploads the repository as a tarball, made at the
  repo root with:

  ```sh
  cd ~/projects/baucord && COPYFILE_DISABLE=1 tar -czf ~/baucord-src.tar.gz \
    --exclude target --exclude ios-target --exclude android-target \
    --exclude .git --exclude .gradle --exclude build --exclude DerivedData \
    --exclude node_modules --exclude out --exclude .DS_Store \
    --exclude local.properties --exclude '*.xcodeproj' \
    --exclude '*.jks' --exclude '*.keystore' --exclude '*.p12' --exclude '*.p8' \
    --exclude '*.pem' --exclude '*.key' --exclude '*.der' --exclude '*.cer' \
    --exclude '*.mobileprovision' --exclude '.env*' \
    --exclude '*.apk' --exclude '*.aab' --exclude '*.ipa' --exclude '*.so' --exclude '*.a' \
    --exclude '*.log' --exclude '*.log.1' --exclude '*.db' --exclude '*.sqlite' .
  ```

  (It leaves out build output, git history, signing material and logs.
  `assets/` must be in it: the build embeds `assets/sounds/*.wav` and
  every platform's icon comes from `assets/icons/icon.png` — the
  desktop's through `scripts/desktop-icons.py`, whose output
  (`icon.ico`, the website's favicon) is committed; the tray's and the
  desktop windows' icon (`tray.png`, and `tray-quiet.png` while muted)
  is the same dog on a disc, made by hand at 64 px, committed too.)
- **Work:** the assistant unpacks it into its sandbox, edits,
  compile-checks every platform it can (host, Android, iOS — see
  SESSION-GUIDE §1), runs the tests, renders every `.slint` change at
  phone and desktop widths, and reviews its own diff.
- **Out:** a `.tar.gz` of the changed files with paths from the repo
  root — cumulative since the last archive the user confirmed applying,
  so one archive is always enough. The user unpacks it at the repo
  root with **`tar -xzmf <archive>`** (`-m`: fresh timestamps, or cargo
  skips the Slint build script). Files to delete are named in the
  hand-over: a tarball cannot delete.
- **Then:** the user runs `cargo +nightly fmt --all` (the repo's
  `.rustfmt.toml` uses nightly-only options), builds on the Mac or
  Windows machine, tries it on the phones and pastes logs back
  (Settings → Debug → Send log puts a phone's log on the server, under
  the identity folder's `logs/`).
- **Docs move with the code:** a decision goes into the matching spec's
  decision log (ANDROID-SPEC §14 D-numbers, IOS-SPEC §14, AEC-SPEC §12,
  PERSISTENCE-SPEC §12); a rule learned goes into SESSION-GUIDE §4; a
  finished plan item leaves its list for one line in PRODUCTION-PLAN
  §7. When something ships, the text that planned it is brought up to
  what was built, so no document carries a plan that is already done.

## 3. The documents, and when to read them

| file | what it is | read it |
|---|---|---|
| `START-HERE.md` | this | first, every time |
| `prompt.md` | the message the user opens a session with | — |
| `SESSION-GUIDE.md` | the working manual: sandbox setup and checks (§1), the crate map (§2), what shipped (§3), rules learned (§4) | every session; §4 by area |
| `PRODUCTION-PLAN.md` | the prioritized list of what is left, the decisions waiting on the user, what was done | every session |
| `PLAY-STORE.md` | the route to Google Play: decisions, account, signing, forms, the closed test, what each step needs | before Play or Android release work |
| `readme.md` | the product, building, running and deploying a server | for deploy/ops, or to know the features |
| `ARCHITECTURE.md` | the runtime: threads, lanes, transport, the server tick, persistence, the client, the hosts | before server, protocol or client-core work |
| `PERSISTENCE-SPEC.md` | the 1.1 design (accounts by device key, invites, roles, history, DMs, files, profiles, admin) — all shipped; §12 is every later decision | before touching admission, roles, history, DMs, files |
| `AEC-SPEC.md` | echo cancellation on every platform; §12 the decisions (E1…) | before audio-path work |
| `ANDROID-SPEC.md` | the Android client as it is (§0–§13); decisions D1…D9 in §1, from D10 on in §14 (from D23 on, newest first) | before Android work |
| `IOS-SPEC.md` | the iPhone client as it is (§0–§13); decisions D1…D13 in §1, from D14 on in §14 (in order) | before iOS work |
| `android/README.md`, `ios/README.md` | building, running and releasing each phone app | when the user builds |
| `macos/README.md` | the Mac app: `baucord.app` in a disk image, signing and notarizing, how its updates work | before touching macOS packaging or the updater |
| `DEVICE-CHECKLIST.md` | what has to be seen on a real Windows machine, Android phone and iPhone before a release: every change the sandbox could not run, as *do this → expect that* | when the user tests; add a line for every platform change made unrun |
| `licenses/README.md` | the third-party notices: what writes them, what CI does | before adding a native library |
| `tools/chat-scroll-sim/` | the chat pane's scrolling harness (ANDROID-SPEC D66); CI runs it on every pull request | before touching the chat's scrolling |

The specs describe what is built at the top and keep a decision log
at the end: why each thing is the way it is, in the order it was
decided. **Where the text and the log disagree, the log is right** —
and the text is then out of date: fix it.

### Reading order by task

- **Server, protocol, admission, persistence** — ARCHITECTURE §3–§5;
  PERSISTENCE-SPEC §0 (rules), §10 (wire), §12 (decisions);
  SESSION-GUIDE §4's first rules (feature bits, the e2e suite).
- **Client UI (Slint)** — SESSION-GUIDE §1 "The loop" and the Slint
  rules in §4; ARCHITECTURE §7.
- **Audio** — AEC-SPEC; ARCHITECTURE §7.3; ANDROID-SPEC D24, D46, D51;
  IOS-SPEC D47.
- **Android** — ANDROID-SPEC §0–§13, then §14 from its newest
  decision; android/README.
- **iPhone** — IOS-SPEC §0–§13, then §14 from its end backwards
  (newest last); ios/README.
- **Hardening and release** — PRODUCTION-PLAN §2–§3, `.github/workflows/`,
  readme's "Deploying a server".
- **Google Play** — PLAY-STORE.md (§2 decisions first), then
  ANDROID-SPEC §11 and D54, android/README §6.

## 4. The rules that matter most

The full list is SESSION-GUIDE §4; these break things when forgotten:

- **The wire is append-only.** Never insert or reorder fields or
  variants (positional postcard); a new message variant needs a **new
  feature bit**; `ALPN` changes only for a deliberate break.
- **The eight public crates share one version** (`protocol`,
  `transport`, `client`, `server`, `frontend`, `desktop`, `android`,
  `ios`); `cargo test -p protocol --test versions` enforces it.
- **A new native library** (C code, bundled data, icons, fonts) goes
  into `COMPONENTS` in `scripts/licenses.py` with its license file in
  `licenses/native/`. Crates need nothing: CI refreshes the notices
  (Settings → Licenses, web/licenses.html) before every build and
  deploy (licenses/README.md).
- **Every new string** goes into all three Fluent catalogs (en, es-AR,
  pt-BR). A string the Slint side shows is also a name in `tr.slint`
  (no text there): `frontend/build.rs` generates its line of `fill_tr`
  and fails the build when a catalog lacks it.
- **Every `.slint` change** is compile-checked and rendered, at phone
  width and at 1000×640. A change to the chat's scrolling, or to
  `app_core::chat`'s row caps, also runs
  `cargo run --manifest-path tools/chat-scroll-sim/Cargo.toml`
  (ANDROID-SPEC D66): it drives the real pane and fails on a wrong
  follow. CI runs it `--locked`, so commit its `Cargo.lock` when the
  run changes it.
- **Phone builds are always `-p android` / `-p ios`**: a workspace build
  unifies the desktop features back in.
- **No worker polls on a short timer, no per-frame log lines.**
- **The server's identity folder must survive every deploy** — it is
  what every client has pinned.

## 5. Where things stand (2026-10-09)

- **Version 1.3.0** in the crates (released: 1.2.1, 2026-10-13, with
  the fixes of the 2026-10-10 review; 1.3.0 carries Settings and the
  admin panel as pages and the 2026-10 review's fixes, PRODUCTION-PLAN.md
  §2a, CHANGELOG.md); the crate map is
  SESSION-GUIDE §2, the list of what it carries SESSION-GUIDE §3. 1.2
  adds what the stores require of a chat app — block, report, delete
  my account, the terms on the connect card (PERSISTENCE-SPEC §12 S1),
  Android's foreground service typed for Play (ANDROID-SPEC D63), the
  third-party notices (Settings → Licenses) — and the chat's
  stickiness by input (D66) and General's Activity view (D65).
- **Desktop**: in production (Windows mostly), self-updating.
- **Android**: voice with the platform echo canceller, chat, DMs,
  files, stream viewing (MediaCodec), Settings → Debug, notifications
  while connected. Sideloaded.
- **iPhone**: the same, with VPIO echo cancellation written but not yet
  confirmed on a device. Ad Hoc builds.
- **Vocabulary**: an *invite code* is what the server mints and a new
  device key needs to get in; an *invite string* (`host:port#SHA256:…`)
  is an address plus the server's fingerprint, which the client still
  accepts to pin a server up front.
- **Server**: persistence on (SQLite), invites and device keys,
  roles, the admin panel.
- **Next**: Google Play's closed test with 1.2.0 (PLAY-STORE.md: the
  Console's foreground-service declaration changes, the Data safety
  form, the account-deletion URL, the listing); then PRODUCTION-PLAN.md
  Milestone A (server and client hardening, signed updates).
