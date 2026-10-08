# baucord — start here

baucord is a voice, text and screen-sharing app in Rust: a self-hosted
relay server (QUIC, no media mixing), a desktop client for Windows,
macOS and Linux, and clients for Android and iPhone that run the same
Rust engine and the same Slint UI. It is in daily use: 1.1.9 on every
platform, desktop users on the in-app updater, Android as a sideloaded
APK, iPhone as an Ad Hoc build, the official server in Docker.

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
  every platform's icon comes from `assets/icons/icon.png`.)
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
  finished plan item moves to PRODUCTION-PLAN §7.

## 3. The documents, and when to read them

| file | what it is | read it |
|---|---|---|
| `START-HERE.md` | this | first, every time |
| `SESSION-GUIDE.md` | the working manual: sandbox setup and checks (§1), the crate map (§2), what shipped (§3), rules learned (§4) | every session; §4 by area |
| `PRODUCTION-PLAN.md` | the prioritized list of what is left, the decisions waiting on the user, what was done | every session |
| `PLAY-STORE.md` | the route to Google Play: decisions, account, signing, forms, the closed test, what each step needs | before Play or Android release work |
| `readme.md` | the product, building, running and deploying a server | for deploy/ops, or to know the features |
| `ARCHITECTURE.md` | the runtime: threads, lanes, transport, the server tick, persistence, the client, the hosts | before server, protocol or client-core work |
| `PERSISTENCE-SPEC.md` | the 1.1 design (accounts by device key, invites, roles, history, DMs, files, profiles, admin) — all shipped; §12 is every later decision | before touching admission, roles, history, DMs, files |
| `AEC-SPEC.md` | echo cancellation on every platform; §12 the decisions (E1…) | before audio-path work |
| `LATENCY-SPEC.md` | the desktop voice latency budget and what could still be cut (LAT-1…9), parked 2026-10-08 | before latency work |
| `ANDROID-SPEC.md` | the Android client; decisions D1…D9 in §1, D10…D62 in §14 (its second table newest first) | before Android work |
| `IOS-SPEC.md` | the iPhone client; decisions D1…D13 in §1, D14…D55 in §14 (in order) | before iOS work |
| `android/README.md`, `ios/README.md` | building, running and releasing each phone app | when the user builds |

The specs keep their first draft at the top and a decision log at the
end. **Where the draft and the log disagree, the log is right** (the
Android draft's "no stream viewing in v1", for instance, was overtaken
by D23).

### Reading order by task

- **Server, protocol, admission, persistence** — ARCHITECTURE §3–§5;
  PERSISTENCE-SPEC §0 (rules), §10 (wire), §12 (decisions);
  SESSION-GUIDE §4's first rules (feature bits, the e2e suite).
- **Client UI (Slint)** — SESSION-GUIDE §1 "The loop" and the Slint
  rules in §4; ARCHITECTURE §7.
- **Audio** — AEC-SPEC; ARCHITECTURE §7.3; ANDROID-SPEC D24, D46, D51;
  IOS-SPEC D47.
- **Android** — ANDROID-SPEC §14's second table (newest decision
  first), then android/README.
- **iPhone** — IOS-SPEC §14 from its end backwards (newest last), then
  ios/README.
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
- **Every new string** goes into all three Fluent catalogs (en, es-AR,
  pt-BR), and every `Tr` property is filled in `fill_tr`.
- **Every `.slint` change** is compile-checked and rendered, at phone
  width and at 1000×640.
- **Phone builds are always `-p android` / `-p ios`**: a workspace build
  unifies the desktop features back in.
- **No worker polls on a short timer, no per-frame log lines.**
- **The server's identity folder must survive every deploy** — it is
  what every client has pinned.

## 5. Where things stand (2026-10-08)

- **Version 1.1.9** everywhere; the crate map is SESSION-GUIDE §2.
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
- **Next**: PRODUCTION-PLAN.md Milestone A (server and client
  hardening, signed updates), then the licence decisions; in parallel,
  Google Play (PLAY-STORE.md) — its decisions are pending.
