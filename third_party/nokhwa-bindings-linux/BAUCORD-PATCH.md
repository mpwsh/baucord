# nokhwa-bindings-linux 0.1.4, vendored for baucord

Upstream: https://github.com/l1npengtul/nokhwa (the
`nokhwa-bindings-linux` crate), crates.io 0.1.4, Apache-2.0 (the full
text is https://www.apache.org/licenses/LICENSE-2.0). Copied from the
registry on 2026-11-13; the workspace root's `[patch.crates-io]` makes
nokhwa's `nokhwa-bindings-linux = "0.1"` resolve here.

A 1.5.1 user on Linux could not turn an ordinary USB webcam on: "Could
not get device property CameraFormat: Failed to Fufill", which means the
list of formats the crate built was empty. The cause there was change
8; changes 1–7 are the other ways that list comes out empty (virtual
cameras, fractional rates).

## The changes (`src/lib.rs`, marked `BAUCORD-PATCH`)

1. **Frame rates are rounded, not dropped** (`fps_of`). V4L2 states a
   rate as a frame interval, seconds a frame. Upstream listed only
   intervals over 1 (`1/30`) and dropped `1001/30000` (29.97 fps); the
   format readback after a set made such a rate an error. Both round.
2. **Ranges of rates are read** (`rates_of`). A stepwise (continuous)
   interval range, as virtual cameras and v4l2loopback report, offers
   its fastest rate and the common rates inside it. Upstream walked
   the numerators and listed rates such as 1 fps, or nothing.
3. **Ranges of sizes are read** (`sizes_of`). A stepwise size range
   offers the common sizes on its grid (and its largest, when no bigger
   than 4K). Upstream paired widths with heights along the diagonal,
   which on a 2..8192 range is 4,000 sizes, none of them common, and
   never the largest; the per-size listing took only the two corners.
4. **YU12 and YV12 are read, as NV12** (`planar_420`, `device_fourcc`,
   `listed_format`, `planar_to_nv12`). nokhwa-core's six pixel formats
   have no planar 4:2:0, which v4l2loopback devices commonly offer. On
   a device with no NV12, its YU12 (or YV12) is listed as NV12, asked
   for by its own FourCC, and each frame's chroma planes are
   interleaved into NV12 on the way out (`frame`). A device that has
   NV12 is read as before.
5. **A current format the crate cannot read is replaced, not an
   error.** `new` read the device's current format first and failed
   when it was YU12 or a fractional rate; now it sets the chosen one.
6. **The rate the driver settles on is kept.** After a set, `new` and
   `set_camera_format` required the readback to equal the request,
   rate included; a camera asked for 30 that runs at 29.97 (or at its
   nearest) was "Rejected". Size and pixel format must still match.
7. **What is left out is said.** A FourCC the crate does not read is
   logged at `info` while listing, and "Failed to Fufill" says how many
   formats were listed.

8. **Only capture nodes are listed, in order** (`query`, `captures`).
   A USB (UVC) camera shows up as two nodes, `/dev/video0` for the
   picture and `/dev/video1` for metadata, under the same name; the
   second has no picture formats. Upstream listed both, in whatever
   order `/dev` gave them, so the first camera (or the one picked by
   name) could be the metadata node, and opening it listed nothing:
   "Failed to Fufill" on an ordinary webcam (the 1.5.1 report; its
   `v4l2-ctl` list was plain YUYV and MJPEG at 1/30 and 1/15). A node
   that cannot be opened to ask is kept.

The crate gained a `log = "0.4"` dependency, and tests for the helpers
(`internal::baucord_tests`; run with `cargo test --lib` in this
folder). Nothing else in `src/` differs from the published crate. Not
copied: `Cargo.lock`, `Cargo.toml.orig`, `.cargo_vcs_info.json`.
