//! Headless encode→decode integration test with synthetic frames.
//!
//! Runs without a display or capture permission, so it works in CI.
//! Skips (with a message) if no H.264 encoder is available in the local
//! FFmpeg build.

use video::codec::{FfmpegDecoder, FfmpegEncoder, KEYFRAME_INTERVAL_MS};
use video::{Frame, Quality};

/// Synthetic RGBA frame: gradient background + a moving white square, so
/// the encoder has real motion to chew on.
fn synth_frame(w: u32, h: u32, i: u64, timestamp_ms: u64) -> Frame {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            data[o] = (x * 255 / w) as u8;
            data[o + 1] = (y * 255 / h) as u8;
            data[o + 2] = 128;
            data[o + 3] = 255;
        }
    }
    // Moving 64×64 white square
    let bx = ((i * 8) % (w as u64 - 64)) as u32;
    let by = ((i * 5) % (h as u64 - 64)) as u32;
    for y in by..by + 64 {
        for x in bx..bx + 64 {
            let o = ((y * w + x) * 4) as usize;
            data[o..o + 3].copy_from_slice(&[255, 255, 255]);
        }
    }
    Frame {
        data,
        width: w,
        height: h,
        timestamp_ms,
    }
}

fn solid_frame(w: u32, h: u32, rgb: [u8; 3], timestamp_ms: u64) -> Frame {
    let mut data = vec![255u8; (w * h * 4) as usize];
    for px in data.chunks_exact_mut(4) {
        px[0] = rgb[0];
        px[1] = rgb[1];
        px[2] = rgb[2];
    }
    Frame {
        data,
        width: w,
        height: h,
        timestamp_ms,
    }
}

#[test]
fn synthetic_roundtrip() {
    let quality = Quality::HD30;
    let mut encoder = match FfmpegEncoder::new(quality) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIP: no usable H.264 encoder in this environment: {e}");
            return;
        }
    };
    let mut decoder = FfmpegDecoder::new().expect("decoder init");

    // ── Phase 1: 90 frames of 1920×1200 (16:10) at ~30fps ──────────
    // Must encode as 1152×720 (aspect-preserving fit into 1280×720).
    let mut aus = 0usize;
    let mut keyframes = 0usize;
    let mut decoded = 0usize;
    for i in 0..90u64 {
        let frame = synth_frame(1920, 1200, i, i * 33);
        for ef in encoder.encode(&frame).expect("encode") {
            assert!(!ef.data.is_empty(), "encoder must never emit empty AUs");
            // The app's chunked transport can't carry frames past
            // 255 × 1080 B; the VBV cap must keep every AU under it.
            assert!(
                ef.data.len() <= 255 * 1_080,
                "AU of {} bytes exceeds the transport per-frame limit",
                ef.data.len()
            );
            assert_eq!(
                (ef.width, ef.height),
                (1152, 720),
                "1920x1200 @ HD30 must fit to 1152x720"
            );
            if ef.is_keyframe {
                keyframes += 1;
            }
            aus += 1;
            if let Some(d) = decoder.decode(&ef).expect("decode") {
                assert_eq!((d.width, d.height), (1152, 720));
                decoded += 1;
                decoder.return_buffer(d.data);
            }
        }
    }
    assert!(aus >= 85, "expected ~90 AUs, got {aus}");
    assert!(decoded >= 85, "expected ~90 decoded frames, got {decoded}");
    // 90 frames × 33ms ≈ 3s of capture time → IDR at t=0 and t≈2s.
    // Allow codec-inserted extras (scene cuts) but require the cadence.
    let expected_min = 1 + (89 * 33) / KEYFRAME_INTERVAL_MS;
    assert!(
        keyframes as u64 >= expected_min && keyframes <= 8,
        "keyframe cadence off: {keyframes} keyframes over ~3s"
    );

    // ── Phase 2: force_keyframe() produces an immediate keyframe ────
    encoder.force_keyframe();
    let frame = synth_frame(1920, 1200, 90, 90 * 33);
    let forced = encoder.encode(&frame).expect("encode");
    assert!(!forced.is_empty());
    assert!(
        forced[0].is_keyframe,
        "force_keyframe must yield a keyframe on the next encode"
    );

    // ── Phase 3: mid-stream source resolution change ────────────────
    // 800×600 fits inside the bound untouched; the decoder must follow
    // without any flush() choreography.
    let mut saw_new_dims = false;
    for i in 91..111u64 {
        let frame = synth_frame(800, 600, i, i * 33);
        for ef in encoder.encode(&frame).expect("encode") {
            assert_eq!((ef.width, ef.height), (800, 600));
            if let Some(d) = decoder.decode(&ef).expect("decode after resize") {
                assert_eq!((d.width, d.height), (800, 600));
                saw_new_dims = true;
                decoder.return_buffer(d.data);
            }
        }
    }
    assert!(saw_new_dims, "decoder never produced frames at the new size");

    // ── Phase 4: colorspace sanity ──────────────────────────────────
    // Encode solid mid-tones and verify they roundtrip close to the
    // original. Catches gross colorspace/range mistakes (601-vs-709 skew
    // shows up as ~10–25 unit channel errors on saturated mid-tones;
    // range mismatch shows up as crushed blacks / gray whites).
    let mut enc2 = FfmpegEncoder::new(quality).expect("encoder 2");
    let mut dec2 = FfmpegDecoder::new().expect("decoder 2");
    for (i, rgb) in [[200u8, 60, 60], [60, 200, 60], [60, 60, 200], [235, 235, 235]]
        .iter()
        .enumerate()
    {
        // A few identical frames each so quantization settles.
        let mut last: Option<Vec<u8>> = None;
        for j in 0..5u64 {
            let ts = (i as u64 * 5 + j) * 33;
            let f = solid_frame(640, 360, *rgb, ts);
            for ef in enc2.encode(&f).expect("encode solid") {
                if let Some(d) = dec2.decode(&ef).expect("decode solid") {
                    last = Some(d.data);
                }
            }
        }
        let data = last.expect("decoded at least one solid frame");
        let center = ((180 * 640 + 320) * 4) as usize;
        for c in 0..3 {
            let got = data[center + c] as i32;
            let want = rgb[c] as i32;
            assert!(
                (got - want).abs() <= 12,
                "channel {c} drifted: encoded {want}, decoded {got} (colorspace/range bug?)"
            );
        }
    }
}
