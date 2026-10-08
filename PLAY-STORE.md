# baucord on Google Play — the plan

Started 2026-10-07. This is the route from today's sideloaded APK
(ANDROID-SPEC D4, D21, D54) to a public listing on Google Play: what
Google requires, where baucord stands against each requirement, the
order to do things in, and who does each step — **you** (account,
identity, money, the Play Console forms), **the assistant** (code,
workflows, drafts of every text), or **both**. The engineering items
live in PRODUCTION-PLAN.md with their ids (MOB-…, LEG-…); this file
is the process that strings them together. Google's rules move:
everything dated here was checked against Google's pages on the date
above (sources in §9) — re-check anything older than a few months
before relying on it.

## 1. Where we stand, in one paragraph

The app is technically close: it targets API 36 (what Play requires of
new apps and updates since 2026-08-31), is 64-bit, passes the 16 KB
page-size check in CI, has no self-updater on Android and asks for no
restricted permission. What it lacks is everything around a *public*
chat app: Play only accepts an **app bundle (AAB)** signed with an
upload key (we build an APK), it requires **report and block** and
**terms of use** for user-generated content, **in-app account
deletion** plus a web page for it, a **privacy policy** and the
**Data safety** form, a **declaration (with a video) of every
foreground-service type**, and — for a new personal developer account —
a **closed test with at least 12 testers opted in for 14 days in a
row** before production can even be requested. Realistically four to
six weeks from today, of which the 14-day test is the fixed part.

## 2. Decisions to take first

Each changes what gets built. A recommendation follows each; the
first two are hard to undo.

1. **Account type** (§3, step 1).
   - *Personal*: you as a person; US$25 once; government ID; an
     Android phone to verify with the Play Console app; Play shows
     your **legal name, country and developer email** on the listing
     (the full address too if the app ever makes money); and the
     **12-tester / 14-day closed test** is mandatory.
   - *Organization*: needs a registered entity and a **D-U-N-S
     number** (free from Dun & Bradstreet, takes days to weeks); shows
     the organization's name, legal address, email and phone; the
     12-tester rule does not apply (it is written for personal
     accounts created after 2023-11-13).
   - *Recommendation*: personal, unless a company already exists — the
     testers are the existing users anyway (§3, step 5).
2. **Who holds the app signing key** (§4).
   - *Decided 2026-10-08*: give Play a copy of **today's release key**
     (the one the sideloaded APKs are signed with), and use the same
     key as the upload key: one key and one password, not two (§4).
     Then a sideloaded install updates in place
     from Play — same signature, keys and history kept (the user taps
     Install on the listing once; Play updates it from then on) — and
     one key is registered for Android's developer verification on
     both channels. Google's default (keys it generates — now a
     "quantum-ready" hybrid scheme) would make every current user
     uninstall (losing their identity unless they export it first) to
     move to the Play build.
   - *Check first*: Play takes only RSA keys of 2048 bits or more.
     `keytool -list -v -keystore baucord-release.jks` prints the
     algorithm and size (ANDROID-SPEC §11 does not record them).
3. **Staying connected in the lobby** (§5).
   - *Recommendation*: a `specialUse` foreground service while
     connected outside a room — the type ntfy (on Play) uses for the
     same job — and `microphone` while in a room; drop `mediaPlayback`.
     Keeps "leave the app open and hear when someone connects" without
     claiming a microphone it no longer opens (D53). If review objects
     to `microphone` for calls, the fallback is the `phoneCall` type
     with Android's Telecom APIs (§5).
4. **Who the reviewers talk to** (§7): the official server with a
   long-lived invite, or a separate review server.
   - *Recommendation*: the official server, a dedicated invite, a
     populated test channel.
5. **Audience and countries.**
   - *Recommendation*: target audience **18+** for the first release
     (keeps the Families policy and minors' requirements out of
     scope; widen later) and all countries. Google's August 2026
     age-restriction rules target random and anonymous chat apps,
     which baucord is not (invite-only, named users). US state
     age-verification laws (Texas's is in force) are a question for
     counsel, not code: Play offers an Age Signals API but does not
     mandate it.
6. **The privacy decisions** (PRODUCTION-PLAN §6, 3; LEG-5): what the
   official server keeps and for how long (history, DMs, files,
   uploaded logs, the 10-second telemetry), and the wording for
   self-hosted servers, whose operators are not you. The privacy
   policy and the Data safety form are written from these answers.
7. **Licences** (LEG-2, LEG-3), the part that touches the phone: the
   Android app links Slint, not FFmpeg. Under Slint's royalty-free
   licence the About panel needs `AboutSlint` (or the listing a "Made
   with Slint" badge), and the APK/AAB needs third-party notices.

## 3. The path, step by step

Steps 1 and 2 run in parallel: the account takes days of waiting on
Google, the engineering takes sessions.

### Step 1 — the developer account (you; start now)

- [ ] Register at play.google.com/console with the Google account that
  will own the app for good (a fresh one dedicated to it is cleaner
  than a personal Gmail). US$25 once, by credit or debit card — no
  prepaid cards. You must be 18+.
- [ ] Verify identity (government ID, through the payments profile)
  and the contact email and phone (one-time codes). Organization:
  D-U-N-S first.
- [ ] Turn on 2-Step Verification for the owning Google account and
  for anyone else added to the Console. The Play App Signing page asks
  for it for every Console user.
- [ ] Personal account: verify access to an Android device with the
  **Play Console mobile app**.
- [ ] Developer verification (Google's new programme for every app,
  on Play or not): apps on Play are registered through Play Console;
  creating the app registers its package name automatically; check
  the Home page once the app exists. The first phase began on
  2026-09-30 in Brazil, Indonesia, Singapore and Thailand, for apps
  installed from participating stores; sideloading is not covered
  yet. It goes global in 2027 — after which a sideloaded APK installs
  normally only if its package name and key are registered to a
  verified developer. Decision 2 makes the Play registration cover the
  GitHub APK too.

### Step 2 — make the app pass review (the assistant; you test)

In this order; each is a PRODUCTION-PLAN item.

- [x] **MOB-5 — the store pipeline** (done 2026-10-08, ANDROID-SPEC
  D62). `bundleRelease` runs beside `assembleRelease`. The AAB and
  the APK for GitHub are both signed with the release key, after the
  build. The AAB is a workflow artifact,
  with the native debug symbols inside (Play's crash reports then show
  Rust frames). Uploads are by hand for now; the Play Developer API
  from CI can come once the app exists.
- [ ] **MOB-11 — the foreground service, for Play**: decision 3's
  types, switched as we enter and leave a room; the
  `startForeground`-refused crash fixed; the wake lock only in a room.
- [ ] **MOB-4 — report and block.** Block: a user's messages and DMs
  hidden for you, their DMs refused by the server. Report: a message
  or a user, with a reason, into a queue in the admin panel, where
  the existing kick/ban/mute act on it.
- [ ] **MOB-14 — delete my account.** In Settings, "Delete my account
  on this server": a new protocol message under a new feature bit;
  the server removes the user's keys, roles, profile, avatar, DMs and
  files, and deletes or anonymizes their messages (decide which); the
  device then forgets that server. Plus a web page saying how, with an
  email address for the official server.
- [ ] **MOB-15 — terms before user content.** On the first connect to a
  server, before the first message: "By continuing you accept the
  rules" with a link to the terms (which define and prohibit
  objectionable content and say reports are acted on). Recorded per
  server.
- [ ] **LEG-5 — privacy policy, terms, account deletion pages** on the
  website (`web/`), public, not a PDF, naming the app as the listing
  does. The assistant drafts; you approve (decision 6). The privacy
  policy is written: `web/privacy.html`, linked from the site's
  footer, signed 0x0 Studios (support@mpw.sh). Its URL is
  `https://baucord.com/privacy.html` once `web.yaml` deploys it.
  LEG-6 lists what would let it promise less.
- [ ] **LEG-2 / LEG-3, phone part**: `AboutSlint` in About; notices
  bundled and shown.
- [ ] Recommended before testers arrive: MOB-8 (font size change ends
  the call), MOB-9 (cellular calls, audio focus), MOB-7 (keep the
  identity key out of device transfer). Play's pre-launch report runs
  the app on real devices and lists crashes.

### Step 3 — create the app and fill the forms (you, from drafts)

- [ ] Create the app: name (30 characters max; "baucord"), default
  language, app (not game), free, no ads. Package name
  **`dev.baucord`**: the `applicationId` every APK so far has shipped
  with (`android/app/build.gradle.kts`), and the iPhone's bundle id.
  It is permanent on Play: it can never be renamed, and never reused,
  even if the app is deleted. Keeping it is what lets today's
  sideloaded installs update from Play (with decision 2's key);
  another name would be a second app beside the first.
- [ ] Play App Signing: change the default to **provide your own key**
  (decision 2; Play Console's PEPK tool encrypts it for upload) — it
  must be settled before any release reaches open testing or
  production; do it before the first upload. The same key is the
  upload key (§4).
- [ ] **App content** page, every section:
  - Privacy policy URL.
  - App access: "restricted": server address, invite code, steps
    (§7).
  - Ads: none.
  - Content rating: the IARC questionnaire (users interact and share
    content, so expect a "Users interact" notice; the age rating
    follows from the answers).
  - Target audience: 18+ (decision 5).
  - Data safety, with the deletion questions (draft in §6).
  - Foreground service declarations: per type, what it does, what the
    user loses if it is delayed or stopped, and a video link (§5).
  - News, health, financial, government: not applicable.
- [ ] **MOB-16 — store listing**: short description (80 characters),
  full description (4,000), app icon 512 × 512 PNG (from
  `assets/icons/icon.png`), feature graphic 1024 × 500 (no alpha),
  phone screenshots — at least 2, better 4+ at 1080 × 1920; no
  "best"/"#1", no store badges — in en, es-AR and pt-BR like the app.
  The assistant renders the screenshots from the UI with demo data
  and writes the texts.

### Step 4 — internal test (both)

An internal test can start before app setup is complete (checked
2026-10-08): no store listing, no forms, possibly no review. It needs
the account, an AAB, the signing choice and, because the app asks for
the microphone, probably a privacy policy URL. The first one, 1.1.9:

1. *Once:* check that the release key is RSA 2048+ (`keytool -list
   -v`). The three `BAUCORD_KEY*` secrets the APK already uses sign
   the bundle too; nothing to add.
2. Publish the v1.1.9 GitHub release as usual. The android-release
   run prints the key's SHA-256. Download
   `baucord-v1.1.9-android.aab` from the run's *Artifacts*.
3. Deploy the site (push `web/` to `main`) so the privacy policy is
   live.
4. Play Console: *Create app* (`dev.baucord`, §3 step 3). Before
   uploading: *Protected with Play → Play Store distribution → Go to
   Play app signing → Change the app signing key*, and choose to
   provide a copy of your own key. Play shows the PEPK command; run it
   on `baucord-release.jks`. Don't create a separate upload key. If
   Play asks for an upload certificate, export this key's
   (android/README.md §6). Then compare the SHA-256 Play shows with
   the one the run printed.
5. *App content → Privacy policy*: the URL. *Test and release →
   Internal testing*: add testers (an email list), create a release,
   upload the AAB, roll out. The opt-in link can take a few hours the
   first time, and testers see a temporary name for up to 48 h.

- [ ] Upload the first AAB to the **internal testing** track (up to 100
  testers by email, available within minutes, lighter review). Install
  it over a sideloaded build on your phone: it must update in place
  (proves decision 2) and keep the identity.
- [ ] Walk the reviewer's path from a clean phone with only the App
  access instructions.

### Step 5 — the closed test, 14 days (you, with the users)

- [ ] A **closed testing** release (reviewed by Google). Testers join
  by an email list or a Google Group and the opt-in link, then install
  from Play.
- [ ] **At least 12 testers opted in for 14 consecutive days** — one
  who leaves early does not count. Recruit 15–20 (the server's
  regulars), and ask them to actually use it: the production
  application asks what testers did and what changed.
- [ ] Ship fixes to the closed track as they come (the clock keeps
  running).

### Step 6 — production (you)

- [ ] Apply for production access on the Dashboard (three groups of
  questions: the closed test, the app, production readiness). Review
  "usually seven days or less".
- [ ] First production release as a **staged rollout** (say 20 %, then
  100 %). Keep the APK on GitHub and the website (same key, same
  version numbers; ANDROID-SPEC D55).

### Step 7 — after launch (both)

- [ ] Releases: the tag builds the APK and the AAB; the AAB goes to
  Play (by API once automated) — same `versionCode` counter for both
  (the run number), so neither channel ever offers a lower build than
  the one installed.
- [ ] Android vitals (crashes, ANRs) — partly covers OPS-2 for Android.
- [ ] The yearly target-API rule: next year's bump will move to API 37
  (likely by August 2027). Known costs: the background-audio rule in
  §5, and the predictive-back opt-out (D29): Google warns that apps
  without predictive back may misbehave "in a future release", with
  no date (PRD-7).
- [ ] Answer reports (MOB-4) on the official server: UGC moderation is
  an ongoing duty, not a feature.

## 4. Signing: the decision that cannot be undone

Play installs what Google signs with the **app signing key**, and
checks each upload against an **upload key**. For a new app Play's default
is keys Google generates (a "quantum-ready" hybrid of a classical key
and a post-quantum one); the alternative, offered until a release
reaches open testing or production, is to give Play a copy of our own
(RSA, 2048 bits or more).

- **Our key (recommended).** Export it with the PEPK tool Play Console
  offers and upload it encrypted. The Play build and the GitHub APK
  share a signature, so either installs over the other with data
  intact, testers move to Play without reinstalling, and developer
  verification sees one key. Cost: Google holds a copy (that is the
  service); we give up Google's hybrid default; and the key stays in
  CI for the APK until we take the APK from Play instead (Play's App
  bundle explorer offers a signed universal APK of each release —
  later, and it also closes REL-3).
- **Google's keys.** Nothing to export, but two signatures in the wild:
  every current user must export their identity (Settings), uninstall
  and reinstall from Play; the GitHub APK and the Play build can never
  update each other.

**One key (decided 2026-10-08).** Our release key is the app signing
key Play holds a copy of, and also the upload key: CI signs the APK and
the bundle with it. That means one set of secrets and one password.
The cost is the one Google points at ("should be different"): a
separate upload key could be reset from Play Console if it leaked or
was lost, and this one cannot. Since this key sits in CI for the APK
anyway, a separate upload key would protect little here.

The app signing key is close to permanent: Play's yearly key upgrade
reaches Android 13 and later, but Android 12 and older keep checking
the original — keep its offline backup.

## 5. Staying connected in the background

Android 14+ makes every foreground service declare a type, and Play
asks for each type in the App content page: what it does, what the
user loses if the system delays or stops it, and a video showing it.
Types must match what the service does.

| state | today (D34, D53) | for Play |
|---|---|---|
| in a voice room | `mediaPlayback` + `microphone`, wake lock | `microphone`, wake lock |
| connected, in the lobby | the same types, no audio open | `specialUse`, no wake lock |
| disconnected | none | none |

- `specialUse` carries a manifest property explaining itself, which
  Play reviews. ntfy, on Play, uses it for exactly this ("the
  foreground notification displays the connectivity status to the
  configurable remote server and the service notifies the user when a
  new message has been published"). Ours: "Keeps the connection to the
  user's own voice server while they wait in its lobby, to show when
  someone starts a call or sends a direct message; baucord servers are
  self-hosted and have no push service."
- `dataSync` does not fit (a daily time limit on Android 15+);
  `remoteMessaging` is for relaying texts between one's devices;
  keeping `microphone` in the lobby would declare a microphone the app
  no longer opens.
- `microphone` for a voice room is defensible — Android's own docs
  give "communication apps" as its use — but Play's declaration form
  files VoIP under the `phoneCall` type ("Calling: Cellular/VoIP/
  Telecom APIs") and lists `microphone` as background audio capture
  (voice commands, recording). Expect a question from review. The
  stronger route, if asked: Android's Telecom self-managed calls
  (`ConnectionService`, `MANAGE_OWN_CALLS`) with the `phoneCall` type —
  which is also how a voice app hears about cellular calls (MOB-9).
- A consequence to accept: a `microphone` service cannot start from
  the background, so after a reconnect in the background the rejoin
  has no microphone until the app is opened (as on iOS, D51).
- At targetSdk 37 (next year's Play requirement), an app in the
  background may play audio only under a foreground service with
  "while-in-use" capabilities — one started while the app was on
  screen. The lobby service usually is; one restarted from the
  background (a reconnect) is not, and then the lobby's DM chime
  (`Notices.playChime`) fails silently. The notification's own sound
  is unaffected.
- Later: push (PRD-1) for people not connected at all.

The video: open the app, join a room, talk, press Home, keep talking,
come back; then leave to the lobby, press Home, have someone join a
room from another device, show the notification. One take per type.

## 6. Data safety: the first draft

Play counts as *collected* anything the app sends off the phone, and
as *shared* anything handed to a third party. Data processed only
ephemerally (in memory, never stored) still goes into the form,
marked as such, and is then left off the public Data safety section;
a transfer the user starts expecting it to reach someone is not
*sharing*. Google says nothing about servers the user chooses or
hosts, so the safe answer is to declare what the app sends to any
baucord server and say in the policy who runs it.

| Play's category | what baucord sends | stored by the server? |
|---|---|---|
| Personal info — name | display name, profile | yes |
| Personal info — user IDs | a SHA-256 of the device key (not the key) | yes |
| Messages — other in-app messages | chat and DMs | yes (history) |
| Photos / files and docs | attachments, avatars | yes |
| Audio — voice | voice to the room | no — relayed in memory, never stored (declare as ephemeral) |
| App info and performance — diagnostics | telemetry every 10 s (app version, OS family and CPU type, link and codec figures, audio device names); logs when the user sends them | telemetry 30 days in the database (and the server log); logs until deleted (LEG-6) |
| App activity — other actions | activity counters (time talking, sharing, watching, online; message count) | yes, until the account is deleted |
| IP address (check Play's guidance on where, if anywhere, it goes; it is not used for location) | seen on every connection | in the server log; with a ban while it lasts |
| Device or other IDs | none beyond the device key | — |

Also: data encrypted in transit — yes (QUIC/TLS); users can request
deletion — yes, once MOB-14 ships; no data shared with third parties;
no ads, no analytics SDKs. To settle with decision 6: whether
telemetry needs consent, and the official server's retention.

## 7. Giving the reviewers access

Everything past the connect screen needs a server and an invite, so the
App access section must carry them, and they must keep working for
every future review (up to five sets of instructions).

- The official server's address and a long-lived invite — an admin
  can mint one with a long expiry and no use cap (PERSISTENCE-SPEC §3);
  it must grant `member`, since a code admits as `guest` by default —
  **MOB-17**.
- A channel with some history and a person reviewers can DM (a bot is
  not needed; a note "DM the user `baucord-team`" is).
- Steps, written against the build that goes to review: add the
  server with the address and the invite code, pick a name, join the
  voice room, send a message, open a DM.
- Keep the server reachable at all times: a rejection for "could not
  access the app" restarts the review.

## 8. Timeline (estimate)

| | duration | depends on |
|---|---|---|
| Account registration and verification | days, sometimes more | Google |
| Step 2 engineering | several sessions | decisions 2, 3, 6 |
| Forms, listing, internal test | 2–3 days | Step 2 |
| Closed test | 14 days minimum | 12+ testers staying opted in |
| Production review | usually ≤ 7 days | Google |

Four to six weeks end to end if the decisions come early.

## 9. Sources (checked 2026-10-07)

- New personal accounts' testing requirement — support.google.com/googleplay/android-developer/answer/14151465
- Target API level — developer.android.com/google/play/requirements/target-sdk
- Developer verification — android-developers.googleblog.com/2026/06/android-developer-verification.html; developer.android.com/developer-verification/guides/android-developer-console
- Registration fee, device verification, age — support.google.com/googleplay/android-developer/answer/6112435
- Personal vs organization accounts — support.google.com/googleplay/android-developer/answer/13628312
- Play App Signing — support.google.com/googleplay/android-developer/answer/9842756
- App bundles required for new apps — android-developers.googleblog.com/2021/06/the-future-of-android-app-bundles-is.html
- Foreground service declarations — support.google.com/googleplay/android-developer/answer/13392821; types: developer.android.com/develop/background-work/services/fgs/service-types; Android 17 background audio — developer.android.com/about/versions/17/changes/bg-audio
- Updates across installers — developer.android.com/google/play/app-updates; universal APK — support.google.com/googleplay/android-developer/answer/9006925
- ntfy's `specialUse` service — github.com/binwiederhier/ntfy-android, `app/src/main/AndroidManifest.xml`
- Account deletion — support.google.com/googleplay/android-developer/answer/13327111
- User-generated content — support.google.com/googleplay/android-developer/answer/9876937
- Data safety — support.google.com/googleplay/android-developer/answer/10787469
- App access — support.google.com/googleplay/android-developer/answer/9859455
- App content declarations — support.google.com/googleplay/android-developer/answer/9815348
- Store listing limits — support.google.com/googleplay/android-developer/answer/13393723; graphics — answer/9866151
- Testing tracks — support.google.com/googleplay/android-developer/answer/9845334
- August 2026 age-restricted chat categories (a "Preview" page, effective 2026-08-26) — support.google.com/googleplay/android-developer/answer/17036597
- Age Signals API (not mandated by Play) — support.google.com/googleplay/android-developer/answer/16569691

## 10. Compliance check against six of Google's pages (2026-10-08)

These are the pages the user sent, checked against the app as of 1.1.9
(`applicationId` `dev.baucord`, decided). **Verdict:** nothing on
these pages blocks us that is not already a planned item. The open
ones are the AAB (MOB-5) and the signing setup (§4). The policy
centre's own requirements are the §3 step 2 list.

| page | what it asks of us | where we stand | to do |
|---|---|---|---|
| Developer Program Policy (the index) | It summarises the policy areas. The detailed rules (UGC, account deletion, permissions, foreground services, Data safety) are the pages in §9. | Already mapped in §3: MOB-4 report and block, MOB-14 account deletion (the Data safety "deletion options" date was 2023-12-07), MOB-15 terms, LEG-5 privacy policy, MOB-11 service types. The manifest still declares `microphone\|mediaPlayback` (decision 3). There are no ads, payments or third-party SDKs. Android has no self-updater and downloads no code. | §3 step 2 as written. In the listing, do not name or look like another chat app (impersonation, store listing policy). LEG-2/LEG-3 attributions and notices. |
| Advance notice to App Review (answer 6320428) | Advance notice is accepted only for 8 cases: third-party IP permission, government, healthcare, Korean gambling, accessibility services, non-profits, law enforcement, regulators. | None of them applies. There is no AccessibilityService in the manifest and no third-party brand assets. | Nothing. Google says other requests may get no reply. |
| Android App Bundle (developer.android.com) | New apps must publish an AAB. Compressed download ≤ 4 GB. No `.obb` files. Sideloaded split APKs fail on Android 10+. | **Not yet**: CI builds an APK only (MOB-5). Size, `.obb` and dynamic resource tables are non-issues. The release build is not debuggable. | MOB-5: `bundleRelease`. Keep the GitHub APK *universal* (`assembleRelease`): never hand out the split APKs Play generates. |
| Play App Signing terms | Publishing an AAB means using Play App Signing. You either give Google your key or it generates one. Google keeps a copy indefinitely. A key handed over cannot be retrieved. Google may generate and optimise APKs "for performance, security and/or size" but "will not change the purpose". Terms can change with notice. | Acceptable. We give our own key (§4) and keep our original. Play's APKs will not be byte-identical to our builds. | Accept them in the Console. |
| Use Play App Signing (answer 9842756) | A new app starts on Google-generated (quantum-ready hybrid) keys. Change to your own **before a release reaches open testing or production**. Own key: **RSA ≥ 2048**. The upload key must be RSA ≥ 2048 and "should be different" from the signing key. 2-Step Verification for every Console user. Register the app-signing fingerprints with any API that checks them. Update `assetlinks.json` for App Links. | Plan matches (§4): our key for both Play and GitHub ("Option 2" on that page), also used as the upload key (allowed: Google says "should", not "must"). **Unverified:** the release key's algorithm and size. No Google sign-in, Maps or App Links (no `autoVerify` filter), so no fingerprints to register. | `keytool -list -v -keystore baucord-release.jks` must say RSA 2048+. Switch to our key before the first upload (allowed until open testing, but internal testers would have to reinstall). Turn on 2SV. |
| Export compliance (answer 113770) | US export law may apply to encryption, including open source. You decide; Google blocks embargoed countries. | Standard cryptography only: TLS 1.3 over QUIC (rustls, ring), SHA-2, device keys. A free mass-market app: ECCN 5D992.c under §740.17(b)(1). Since BIS's 2021-03-29 rule this needs no licence and no self-classification report, and an email notification only for *non-standard* crypto. BIS's page still describes that notification for public source code. | Nothing to file for Play. If the source goes public, the BIS/NSA email is a few lines and cheap insurance. Not legal advice. The iOS side stays MOB-6. |

Sources: play.google/developer-content-policy; support.google.com/googleplay/android-developer/answer/6320428, /9842756, /113770; developer.android.com/guide/app-bundle; play.google.com/about/play-app-signing-terms.html; bis.gov/learn-support/encryption-controls/encryption-items-not-subject-to-ear; thompsonhine.com/publications/bis-eliminates-most-mass-market-encryption-reporting-obligations.
