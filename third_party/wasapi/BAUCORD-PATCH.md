# wasapi 0.24.0, vendored for baucord

Upstream: https://github.com/HEnquist/wasapi-rs, crates.io `wasapi`
0.24.0, MIT (LICENSE.txt). Copied from the registry on 2026-10-07; the
workspace root's `[patch.crates-io]` makes every `wasapi = "0.24"` in
the tree resolve here.

## The change

One method on `AudioClient` in `src/api.rs`:

```rust
pub fn as_iaudioclient(&self) -> &IAudioClient {
    &self.client
}
```

Why: Windows lowers other apps while a Communications-category stream
runs (the "communications activity" ducking). The per-stream opt-out,
`IAudioClientDuckingControl::SetDuckingOptionsForCurrentStream(
AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS)`, has to be obtained
from the stream's own `IAudioClient` through `GetService`, and the
crate keeps that field private (0.25.0 too). `crates/audio/src/native/
wasapi.rs` does the rest (AEC-SPEC.md E11).

Also removed from the copy: `examples/`, the dev-dependencies, the
upstream `.github/`, `Cargo.lock` and `Cargo.toml.orig`. Nothing else
in `src/` differs from the published crate.

## Leaving this behind

Upstream an accessor (or a `get_ducking_control()` wrapper) and, once a
release has it, delete this folder, the `[patch.crates-io]` entry and
the `exclude` in the root `Cargo.toml`, and bump `wasapi` in
`crates/audio/Cargo.toml`. To move to a newer upstream in the meantime:
copy the new crate here and re-apply the method above.
