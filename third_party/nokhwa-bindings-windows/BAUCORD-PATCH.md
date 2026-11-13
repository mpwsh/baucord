# nokhwa-bindings-windows 0.4.6, vendored for baucord

Upstream: https://github.com/l1npengtul/nokhwa (the
`nokhwa-bindings-windows` crate), crates.io 0.4.6, Apache-2.0 (the
full text is https://www.apache.org/licenses/LICENSE-2.0). Copied from
the registry on 2026-11-13; the workspace root's `[patch.crates-io]`
makes nokhwa's `nokhwa-bindings-windows = "0.4"` resolve here.

## The changes (`src/lib.rs`, marked `BAUCORD-PATCH`)

1. **Frame rates are rounded, not dropped.** Media Foundation states
   a rate as a fraction. Upstream kept only those over 1 and turned
   every other one (30000/1001 for 29.97 fps, 10000000/333333 for
   30 fps) into 0, and a format whose rates were all 0 was not listed.
   A camera that states every rate that way — common for capture
   cards, virtual cameras and phone-as-webcam apps — listed nothing,
   and opening it failed with "Failed to fulfill requested format"
   (2026-11-13, a 1.5.0 user on Windows). `rate_of` rounds the
   fraction instead, in `compatible_format_list` and in `set_format`'s
   matching alike, so a listed format can also be set.
2. **Skipped formats are logged.** A subtype the crate does not read
   (I420, RGB32, H.264…) is still left out, but `compatible_format_list`
   now says so at `info`, with its FourCC and size, so a user's log
   shows what their camera offers when nothing fits.

The crate gained a `log = "0.4"` dependency for the second change.
Nothing else in `src/` differs from the published crate. Not copied:
`Cargo.lock`, `Cargo.toml.orig`, `.cargo_vcs_info.json`.
