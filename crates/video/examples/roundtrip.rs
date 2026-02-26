use screencap::capture::ScreenCapturer;
use screencap::Quality;
use std::time::Instant;

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
    println!(
        "Roundtrip benchmark: {} — {} frames [{}]",
        quality, num_frames, backend
    );

    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");

    let (mut encoder, mut decoder) = {
        let enc =
            screencap::codec::FfmpegEncoder::new(quality).expect("Failed to create ffmpeg encoder");
        println!("  Encoder: {}", enc.encoder_name());
        let dec = screencap::codec::FfmpegDecoder::new().expect("Failed to create ffmpeg decoder");
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

        // Encode
        let t = Instant::now();
        let encoded = encoder.encode(&frame).expect("Encode failed");
        total_encode += t.elapsed();
        total_bytes += encoded.data.len();

        // Decode
        let t = Instant::now();
        let decoded = decoder.decode(&encoded).expect("Decode failed");
        total_decode += t.elapsed();

        if decoded.is_some() {
            decoded_count += 1;
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
    let max_fps = num_frames as f64 / total_time.as_secs_f64();

    println!("--- Results ({}) [{}] ---", quality, backend);
    println!("  Avg capture:  {:?}", avg_capture);
    println!("  Avg encode:   {:?}", avg_encode);
    println!("  Avg decode:   {:?}", avg_decode);
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
        total_bytes as f64 * 8.0 / (num_frames as f64 / quality.fps() as f64) / 1_000_000.0
    );
    println!("  Max pipeline FPS: {:.1}", max_fps);
    println!("  Decoded:      {}/{}", decoded_count, num_frames);
}

