//! Headless encode→decode integration test with synthetic frames.
//!
//! Runs without a display or capture permission, so it works in CI.
//! Skips (with a message) if no H.264 encoder is available in the local
//! `FFmpeg` build.

use video::{
    codec::{FfmpegDecoder, FfmpegEncoder, KEYFRAME_INTERVAL_MS},
    Frame, PixelFormat, Quality,
};

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
    let bx = ((i * 8) % (u64::from(w) - 64)) as u32;
    let by = ((i * 5) % (u64::from(h) - 64)) as u32;
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
        format: PixelFormat::Rgba,
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
        format: PixelFormat::Rgba,
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
    assert!(
        saw_new_dims,
        "decoder never produced frames at the new size"
    );

    // ── Phase 4: colorspace sanity ──────────────────────────────────
    // Encode solid mid-tones and verify they roundtrip close to the
    // original. Catches gross colorspace/range mistakes (601-vs-709 skew
    // shows up as ~10–25 unit channel errors on saturated mid-tones;
    // range mismatch shows up as crushed blacks / gray whites).
    let mut enc2 = FfmpegEncoder::new(quality).expect("encoder 2");
    let mut dec2 = FfmpegDecoder::new().expect("decoder 2");
    for (i, rgb) in [
        [200u8, 60, 60],
        [60, 200, 60],
        [60, 60, 200],
        [235, 235, 235],
    ]
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
            let got = i32::from(data[center + c]);
            let want = i32::from(rgb[c]);
            assert!(
                (got - want).abs() <= 12,
                "channel {c} drifted: encoded {want}, decoded {got} (colorspace/range bug?)"
            );
        }
    }
}

/// What iOS's decoder receives (IOS-SPEC.md §14 D30): the real encoder's
/// access units, re-framed by `annexb::split` into parameter sets plus
/// length-prefixed NAL units. Rebuilt into Annex-B and decoded, they must
/// give the same pictures as the originals — so the re-framing loses and
/// adds nothing, on this machine's actual encoder output.
#[test]
fn avcc_reframing_keeps_every_picture() {
    use video::annexb;

    let mut encoder = match FfmpegEncoder::new(Quality::HD30) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIP: no usable H.264 encoder in this environment: {e}");
            return;
        }
    };
    let mut direct = FfmpegDecoder::new().expect("decoder init");
    let mut reframed = FfmpegDecoder::new().expect("decoder init");
    let (mut keyframes, mut compared) = (0, 0);
    for i in 0..75u64 {
        let frame = synth_frame(1280, 720, i, i * 33);
        for ef in encoder.encode(&frame).expect("encode") {
            let split = annexb::split(&ef.data, Vec::new());
            if ef.is_keyframe {
                keyframes += 1;
                assert!(
                    !split.sps.is_empty() && !split.pps.is_empty(),
                    "keyframe without in-band SPS/PPS: {}",
                    video::describe_annexb_nals(&ef.data)
                );
            }
            assert!(
                !split.avcc.is_empty(),
                "an access unit with no picture data"
            );

            // AVCC back to Annex-B: parameter sets, then each
            // length-prefixed unit behind a start code.
            let mut rebuilt = Vec::new();
            for unit in split.sps.iter().chain(&split.pps) {
                rebuilt.extend_from_slice(&[0, 0, 0, 1]);
                rebuilt.extend_from_slice(unit);
            }
            let mut rest = split.avcc.as_slice();
            while !rest.is_empty() {
                let (len, tail) = rest.split_at(annexb::AVCC_LENGTH_BYTES);
                let len = u32::from_be_bytes(len.try_into().unwrap()) as usize;
                assert!(len > 0 && len <= tail.len(), "bad length prefix {len}");
                rebuilt.extend_from_slice(&[0, 0, 0, 1]);
                rebuilt.extend_from_slice(&tail[..len]);
                rest = &tail[len..];
            }
            let again = video::EncodedFrame {
                data: rebuilt,
                timestamp_ms: ef.timestamp_ms,
                is_keyframe: ef.is_keyframe,
                width: ef.width,
                height: ef.height,
            };
            let a = direct.decode(&ef).expect("decode original");
            let b = reframed.decode(&again).expect("decode re-framed");
            match (a, b) {
                (Some(a), Some(b)) => {
                    assert_eq!((a.width, a.height), (b.width, b.height));
                    assert!(a.data == b.data, "frame {i}: pictures differ");
                    compared += 1;
                }
                (None, None) => {}
                _ => panic!("frame {i}: one decoder produced a picture, the other not"),
            }
        }
    }
    assert!(keyframes >= 1, "no keyframe in 75 frames");
    assert!(compared >= 70, "only {compared} pictures compared");
}
