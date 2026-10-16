# Third-party notices

What baucord ships is listed, with each part's license, in the apps
(Settings → Licenses, from `crates/frontend/licenses.json`) and on the
site (`web/licenses.html`). `scripts/licenses.py` writes both:

- the Rust crates every shipped build links, from `cargo about` (the
  targets and the license chosen for multi-licensed crates are in
  `about.toml` at the repository root);
- the code cargo cannot see, with its license file here in `native/`,
  each taken from the project's own repository: Skia and what it bundles
  (libjpeg-turbo, libpng, zlib, Wuffs, FreeType, ICU data), FFmpeg and
  x264 in the desktop builds (with the GPL texts), the NVIDIA and AMD
  encoder headers in FFmpeg's Windows build, libopus, the Lucide icons,
  and Slint's royalty-free license (its crates carry no license file);
- the website's own: Alpine.js (`web/js/alpine.min.js`), on the site's
  page only, since the apps do not contain it.

Our own crates — every package under `crates/` — are left out.

Nobody has to run it. `.github/workflows/licenses.yml` runs it from
the commit being built, and every workflow that ships something calls
that first — the desktop release, the Android and iOS releases, the
website deploy — then lays its output over the committed copies before
building. So every platform and the site carry the same list, made from
the same `Cargo.lock`. The committed copies are only what a local build
shows; refreshing them is optional:

    cargo install cargo-about --features cli   # once
    python3 scripts/licenses.py

A new native library (a C dependency, bundled data, an icon or font
set) is the one manual step: it goes into `COMPONENTS` in the script,
with its license file here.

Two licenses ask for more than the text:

- **Slint** (royalty-free license 2.0): the `AboutSlint` widget in
  Settings → About.
- **FFmpeg and x264** (GNU GPL 2.0 or later, desktop builds only): a
  desktop build as a whole is distributed under the GNU GPL version 3,
  and the exact source of each is named — FFmpeg 7.1.2 (`FFMPEG_VERSION`,
  checked against `FFMPEG_SHA256`; the pinned vcpkg's on Windows) and
  x264 at `X264_COMMIT` in `.github/workflows/release.yml`. Changing
  either means changing the `COMPONENTS` notes too.
