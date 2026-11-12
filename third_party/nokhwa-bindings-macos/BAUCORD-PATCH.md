# nokhwa-bindings-macos 0.2.4, vendored for baucord

Upstream: https://github.com/l1npengtul/nokhwa (the `nokhwa-bindings-macos`
crate), crates.io 0.2.4, Apache-2.0 (the license headers in `src/`
and `build.rs`; the full text is
https://www.apache.org/licenses/LICENSE-2.0). Copied from the registry
on 2026-11-12; the workspace root's `[patch.crates-io]` makes nokhwa's
`nokhwa-bindings-macos = "0.2"` resolve here.

## The change

One loop in `AVCaptureDevice::set_all` (`src/lib.rs`, marked
`BAUCORD-PATCH`): the `AVCaptureDeviceFormat` it activates and the
frame-rate range whose duration it then sets are taken from the same
format, and the search stops at the first match.

Why: upstream kept the *last* format of the asked size and a range
from *any* format of that size. A Logitech C920 on macOS lists several
formats at 1280×720; activating one and then setting a frame duration
that only another one supports makes `setActiveVideoMinFrameDuration:`
throw, and `objc`'s `exception` feature turns the `NSException` into a
panic on the camera thread (2026-11-12, the first build that opened
cameras at 720p). At 640×360 the formats happened to agree, so the
640×360-only 1.5 build never met it.

Nothing else in `src/` differs from the published crate. Also not
copied: `Cargo.lock`, `Cargo.toml.orig`, `.gitignore`,
`.cargo_vcs_info.json`.

Related, worked around in `crates/video/src/camera.rs` rather than
patched: nokhwa 0.10's `AVFoundation` backend lists, under each pixel
format, the sizes of the *other* formats (`compatible_list_by_resolution`
filters with `!=`), so `mac_formats` reads them another way.

## Leaving this behind

Upstream the fix and, once a release has it, delete this folder and
the `[patch.crates-io]` entry in the root `Cargo.toml`. To move to a
newer upstream in the meantime: copy the new crate here and re-apply
the loop above.
