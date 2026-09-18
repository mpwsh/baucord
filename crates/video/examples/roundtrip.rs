use std::time::Instant;

use video::{capture::ScreenCapturer, Quality};

fn main() {
    env_logger::init();

    let quality = match std::env::args().nth(1).as_deref() {
        Some("hd30") => Quality::HD30,
        Some("hd60") => Quality::HD60,
        Some("fhd30") => Quality::FHD30,
        Some("fhd60") => Quality::FHD60,
        _ => {
            println!("Usage: roundtrip [hd30|hd60|fhd30|fhd60]");
            println!("Defaulting to HD30");
            Quality::HD30
        }
    };

    let num_frames = 60u32;

    let backend = "ffmpeg";
    println!("Roundtrip benchmark: {quality} — {num_frames} frames [{backend}]");

    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");

    let (mut encoder, mut decoder) = {
        let enc =
            video::codec::FfmpegEncoder::new(quality).expect("Failed to create ffmpeg encoder");
        println!("  Encoder: {}", enc.encoder_name());
        let dec = video::codec::FfmpegDecoder::new().expect("Failed to create ffmpeg decoder");
        (enc, dec)
    };

    let mut total_capture = std::time::Duration::ZERO;
    let mut total_encode = std::time::Duration::ZERO;
    let mut total_decode = std::time::Duration::ZERO;
    let mut total_bytes = 0usize;
    let mut decoded_count = 0u32;

    for i in 0..num_frames {
        // Capture
        let t = Instant::now();
        let frame = capturer.capture().expect("Capture failed");
        total_capture += t.elapsed();

        // Encode (zero or more access units per input frame)
        let t = Instant::now();
        let encoded_frames = encoder.encode(&frame).expect("Encode failed");
        total_encode += t.elapsed();

        // Decode each access unit
        for encoded in &encoded_frames {
            total_bytes += encoded.data.len();
            let t = Instant::now();
            let decoded = decoder.decode(encoded).expect("Decode failed");
            total_decode += t.elapsed();
            if decoded.is_some() {
                decoded_count += 1;
            }
        }

        if i % 10 == 0 {
            print!(".");
        }
    }
    println!();

    let avg_capture = total_capture / num_frames;
    let avg_encode = total_encode / num_frames;
    let avg_decode = total_decode / num_frames;
    let avg_frame_bytes = total_bytes / num_frames as usize;
    let total_time = total_capture + total_encode + total_decode;
    let max_fps = f64::from(num_frames) / total_time.as_secs_f64();

    println!("--- Results ({quality}) [{backend}] ---");
    println!("  Avg capture:  {avg_capture:?}");
    println!("  Avg encode:   {avg_encode:?}");
    println!("  Avg decode:   {avg_decode:?}");
    println!(
        "  Avg frame:    {} bytes ({:.1} KB)",
        avg_frame_bytes,
        avg_frame_bytes as f64 / 1024.0
    );
    println!(
        "  Total bytes:  {} ({:.2} MB)",
        total_bytes,
        total_bytes as f64 / 1_048_576.0
    );
    println!(
        "  Avg bitrate:  {:.2} Mbps",
        total_bytes as f64 * 8.0 / (f64::from(num_frames) / f64::from(quality.fps())) / 1_000_000.0
    );
    println!("  Max pipeline FPS: {max_fps:.1}");
    println!("  Decoded:      {decoded_count}/{num_frames}");
}
