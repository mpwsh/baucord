# baucord for macOS — the app and its disk image

The desktop client on a Mac ships as **`baucord.app`**: one universal
bundle (Apple silicon and Intel) in a disk image,
`baucord-v<version>-macos.dmg`, which opens to the app and an
Applications shortcut to drag it onto. Being a real app, it opens its
window and nothing else: the bare `baucord` binary in the old zips
opened a Terminal window first, because that is what macOS does with a
command-line program.

| file | what | who uses it |
|---|---|---|
| `Info.plist` | the bundle's Info.plist; `package.sh` writes the version in | — |
| `baucord.entitlements` | what the hardened runtime allows: the microphone, nothing else | Developer ID signing |
| `package.sh` | binaries → `baucord.app` → the `.dmg` and the updater's `.zip`; signs and notarizes | `release.yml`, job `macos-app` |

## 1. What a release publishes for macOS

| asset | for |
|---|---|
| `baucord-v<v>-macos.dmg` | new installs: the website's macOS download |
| `baucord-v<v>-macos.zip` | the updater of a copy installed as `baucord.app` |
| `baucord-v<v>-macos-arm64.zip`, `-macos-intel.zip` | as before: the bare binaries and the server, for copies installed from them, whose updater still fetches these |

The updater tells the two kinds of install apart by where it runs. In
`…/baucord.app/Contents/MacOS/` it downloads the bundle zip into a
hidden `.baucord-update` folder beside the app, unpacks it there and
swaps the whole bundle in (two renames; on a failure the old app goes
back). A bundle is signed as one piece: replacing only its executable
would break the signature, and the permissions macOS ties to it —
microphone, screen recording, Accessibility — would be asked for again
after every update. Anywhere else it updates the bare binary, as it
always has.

Settings and the identity key live in the same folder for both, so
moving from the zip to the app keeps your servers and identity.

## 2. Signing and notarizing (once, needs the paid Developer Program)

Without these secrets the workflow still builds the app and the disk
image, signed **ad hoc**: on macOS 15 and later, users then have to
open System Settings → Privacy & Security → **Open Anyway** the first
time. With them, the app and the disk image are signed with your
Developer ID and, on a release, **notarized** by Apple: they open like
any app from the web.

1. A **Developer ID Application** certificate. Only the Account Holder
   can make one: Xcode → Settings → Accounts → your team → Manage
   Certificates → **+** → *Developer ID Application*. (Not *Apple
   Distribution*: that one is for the App Store and TestFlight.)
2. Export it with its key: Keychain Access → login → My Certificates →
   *Developer ID Application: …* → right-click → Export → `.p12`, with
   a password.
3. Two repository secrets:
   ```sh
   R=mpwsh/baucord
   base64 < baucord-developer-id.p12 | tr -d '\n' | gh secret set MACOS_DEVELOPER_ID_P12_BASE64 -R $R
   gh secret set MACOS_DEVELOPER_ID_PASSWORD -R $R    # prompts: the .p12's password
   ```

Notarizing uses the App Store Connect API key already set up for
TestFlight (`ASC_API_KEY_P8_BASE64`, `ASC_API_KEY_ID`,
`ASC_API_ISSUER_ID`; ios/README.md §3c). It runs on releases only — it
waits on Apple, a few minutes per file — and a refusal fails the run
with Apple's log of why.

## 3. Trying a build

Every push to `main` runs the job too (signed if the secrets are there,
never notarized) and keeps the disk image for 7 days as the artifact
`macos-dmg-<commit>` of the *Build and Release* run. A copy from there
is not notarized: the first open needs **Open Anyway**, as above.

## 4. What to know

- **Where it may live:** updating writes beside the app, so it needs a
  folder the user can write: `/Applications` for an administrator, or
  `~/Applications`. Run from the disk image itself (read-only) or from
  a standard account's `/Applications`, the update fails with a message
  and the app stays as it is.
- **Ad hoc and updates:** with no Developer ID, macOS may ask the user
  to allow baucord to "modify apps" the first time it updates itself
  (System Settings → Privacy & Security → App Management). A Developer
  ID–signed app updating itself is not asked.
- **No App Store build:** the Mac App Store requires the sandbox, and
  the desktop's video (FFmpeg with x264, GPL-3.0) and keybinds
  (Accessibility) do not fit it as they are (PRODUCTION-PLAN.md, the
  2026-10-24 entry in §7).
- **Minimum:** macOS 11, as the binaries are built for.
