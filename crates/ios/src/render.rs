//! Which renderer draws the app (IOS-SPEC.md §14 D40).
//!
//! The first iPhone run showed text drawn wrong now and then: in the
//! lines that change every two seconds (the status, the latency) and in
//! text that had not changed at all (a member count), whole glyphs had
//! their top half missing or replaced by a block. The strings are
//! right, and a glyph that did not change cannot be laid out wrong: it
//! is the glyph *image* that is wrong — Skia's glyph atlas on the GPU
//! (in Slint 1.18 Skia draws on iOS through wgpu's Metal backend), or
//! the rasteriser that filled it.
//!
//! Which one is a question of renderers, so a launch can name another
//! one (`ios/run.sh --renderer …`, read from the environment of that
//! launch only — the app opened from the home screen draws as normal):
//!
//! - `skia-software`: Skia on the CPU — the same glyph rasteriser, no
//!   GPU atlas. The glitch gone here means the GPU path.
//! - `software`: Slint's own renderer, its own rasteriser. The glitch
//!   still there with `skia-software` but gone here means Skia's glyphs.

/// The renderer to select: Skia on the GPU unless the launch asked for
/// another.
pub fn renderer_name() -> String {
    match std::env::var("BAUCORD_RENDERER") {
        Ok(name) if !name.trim().is_empty() => {
            let name = name.trim().to_string();
            log::warn!("renderer: {name} (BAUCORD_RENDERER — a diagnostic launch)");
            name
        }
        _ => "skia".to_string(),
    }
}
