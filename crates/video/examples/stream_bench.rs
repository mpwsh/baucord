use std::time::{Duration, Instant};

use video::{capture::ScreenCapturer, codec::FfmpegEncoder, Quality};

fn main() {
    env_logger::init();

    let quality = match std::env::args().nth(1).as_deref() {
        Some("hd60") => Quality::HD60,
        Some("fhd30") => Quality::FHD30,
        Some("fhd60") => Quality::FHD60,
        _ => Quality::HD30, // the default, "hd30" included
    };

    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    println!("Stream benchmark: {quality} for {seconds}s");

    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");
    let mut encoder = FfmpegEncoder::new(quality).expect("Failed to create encoder");

    println!("Encoder: {}", encoder.encoder_name());

    let frame_duration = quality.frame_duration();
    let run_duration = Duration::from_secs(seconds);
    let start = Instant::now();
    let mut frames_encoded = 0u64;
    let mut frames_dropped = 0u64;
    let mut total_bytes = 0usize;
    let mut second_frames = 0u32;
    let mut second_bytes = 0usize;
    let mut second_start = Instant::now();

    while start.elapsed() < run_duration {
        let frame_start = Instant::now();

        let frame = match capturer.capture() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Capture error: {e}");
                frames_dropped += 1;
                continue;
            }
        };

        let encoded_frames = match encoder.encode(&frame) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Encode error: {e}");
                frames_dropped += 1;
                continue;
            }
        };

        let mut had_keyframe = false;
        for encoded in &encoded_frames {
            total_bytes += encoded.data.len();
            second_bytes += encoded.data.len();
            had_keyframe |= encoded.is_keyframe;
        }
        second_frames += encoded_frames.len() as u32;
        frames_encoded += encoded_frames.len() as u64;

        if second_start.elapsed() >= Duration::from_secs(1) {
            let mbps = second_bytes as f64 * 8.0 / 1_000_000.0;
            let avg_kb = if second_frames > 0 {
                second_bytes as f64 / f64::from(second_frames) / 1024.0
            } else {
                0.0
            };
            println!(
                "  [{:.0}s] {} fps, {:.2} Mbps, {:.1} KB/frame{}",
                start.elapsed().as_secs_f64(),
                second_frames,
                mbps,
                avg_kb,
                if had_keyframe { " [K]" } else { "" },
            );
            second_frames = 0;
            second_bytes = 0;
            second_start = Instant::now();
        }

        let elapsed = frame_start.elapsed();
        if elapsed < frame_duration {
            std::thread::sleep(frame_duration.checked_sub(elapsed).unwrap());
        } else {
            frames_dropped += 1;
        }
    }

    let total_secs = start.elapsed().as_secs_f64();
    println!("\n--- Summary ({quality}) ---");
    println!("  Encoder:  {}", encoder.encoder_name());
    println!("  Encoded:  {frames_encoded} frames in {total_secs:.1}s");
    println!("  Dropped:  {frames_dropped} frames");
    println!("  Avg FPS:  {:.1}", frames_encoded as f64 / total_secs);
    println!("  Total:    {:.2} MB", total_bytes as f64 / 1_048_576.0);
    println!(
        "  Bitrate:  {:.2} Mbps",
        total_bytes as f64 * 8.0 / total_secs / 1_000_000.0
    );
}
