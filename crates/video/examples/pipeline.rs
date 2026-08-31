//! End-to-end screen sharing pipeline benchmark.
//!
//! Simulates the full path: capture → encode → network → decode → rescale
//! with per-stage timing to identify the actual bottleneck.
//!
//! ```
//! cargo run --release --example pipeline_bench
//! cargo run --release --example pipeline_bench -- hd60 10
//! cargo run --release --example pipeline_bench -- fhd30 5 1920x1200
//! ```

use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use video::{
    capture::ScreenCapturer,
    codec::{FfmpegDecoder, FfmpegEncoder},
    EncodedFrame, Quality,
};

// ---------------------------------------------------------------------------
// Stage timings — one per frame
// ---------------------------------------------------------------------------

struct FrameTimings {
    capture_us: u64,
    encode_us: u64,
    network_us: u64,
    decode_us: u64,
    rescale_us: u64,
    frame_bytes: usize,
    is_keyframe: bool,
}

// ---------------------------------------------------------------------------
// Simulated network relay
// ---------------------------------------------------------------------------

/// Simulate a network hop: fixed latency + bandwidth throttle.
///
/// At 300KB budget per tick (typical renet config), a 10KB frame
/// "costs" ~1ms of serialization overhead. A 50KB keyframe costs ~5ms.
fn simulate_network(
    encoded: &EncodedFrame,
    latency_ms: u64,
    bandwidth_bytes_per_sec: usize,
) -> u64 {
    let t = Instant::now();

    // Fixed relay latency (server processing + forwarding)
    if latency_ms > 0 {
        thread::sleep(Duration::from_millis(latency_ms));
    }

    // Bandwidth throttle: how long would this frame take to transmit?
    if bandwidth_bytes_per_sec > 0 {
        let tx_time_us = (encoded.data.len() as u64 * 1_000_000) / bandwidth_bytes_per_sec as u64;
        if tx_time_us > 0 {
            thread::sleep(Duration::from_micros(tx_time_us));
        }
    }

    t.elapsed().as_micros() as u64
}

// ---------------------------------------------------------------------------
// Simulated viewer rescale
// ---------------------------------------------------------------------------

/// Simulate rescaling decoded RGBA to a viewer's window size.
///
/// Uses a simple nearest-neighbor blit to measure the memory bandwidth cost.
/// In production you'd use swscale or GPU compositing.
fn rescale_nearest(src: &[u8], src_w: u32, src_h: u32, dst: &mut Vec<u8>, dst_w: u32, dst_h: u32) {
    let size = (dst_w * dst_h * 4) as usize;
    dst.resize(size, 0);

    for y in 0..dst_h {
        let src_y = (y as u64 * src_h as u64 / dst_h as u64) as u32;
        for x in 0..dst_w {
            let src_x = (x as u64 * src_w as u64 / dst_w as u64) as u32;
            let si = ((src_y * src_w + src_x) * 4) as usize;
            let di = ((y * dst_w + x) * 4) as usize;
            dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline modes
// ---------------------------------------------------------------------------

/// Single-threaded pipeline: capture → encode → network → decode → rescale
/// all on one thread, sequentially. This is what you have today.
fn run_serial(
    quality: Quality,
    num_seconds: u64,
    viewer_w: u32,
    viewer_h: u32,
    net_latency_ms: u64,
    net_bandwidth: usize,
) -> Vec<FrameTimings> {
    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");
    let mut encoder = FfmpegEncoder::new(quality).expect("Failed to create encoder");
    let mut decoder = FfmpegDecoder::new().expect("Failed to create decoder");
    let mut rescale_buf: Vec<u8> = Vec::new();
    let mut timings = Vec::new();

    println!("  Encoder: {}", encoder.encoder_name());

    let deadline = Instant::now() + Duration::from_secs(num_seconds);

    while Instant::now() < deadline {
        // Capture
        let t = Instant::now();
        let frame = match capturer.capture() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("  capture error: {e}");
                continue;
            }
        };
        let capture_us = t.elapsed().as_micros() as u64;

        // Encode (zero or more access units per input frame)
        let t = Instant::now();
        let encoded_frames = match encoder.encode(&frame) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  encode error: {e}");
                continue;
            }
        };
        let encode_us = t.elapsed().as_micros() as u64;

        for encoded in &encoded_frames {
            let frame_bytes = encoded.data.len();
            let is_keyframe = encoded.is_keyframe;

            // Network
            let network_us = simulate_network(encoded, net_latency_ms, net_bandwidth);

            // Decode
            let t = Instant::now();
            let decoded = match decoder.decode(encoded) {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("  decode error: {e}");
                    continue;
                }
            };
            let decode_us = t.elapsed().as_micros() as u64;

            // Rescale to viewer window
            let t = Instant::now();
            rescale_nearest(
                &decoded.data,
                decoded.width,
                decoded.height,
                &mut rescale_buf,
                viewer_w,
                viewer_h,
            );
            let rescale_us = t.elapsed().as_micros() as u64;

            decoder.return_buffer(decoded.data);

            timings.push(FrameTimings {
                capture_us,
                encode_us,
                network_us,
                decode_us,
                rescale_us,
                frame_bytes,
                is_keyframe,
            });
        }
    }

    timings
}

/// Two-threaded pipeline: capture+encode on thread A, network+decode+rescale
/// on thread B. This overlaps capture wait with decode/rescale.
fn run_threaded(
    quality: Quality,
    num_seconds: u64,
    viewer_w: u32,
    viewer_h: u32,
    net_latency_ms: u64,
    net_bandwidth: usize,
) -> Vec<FrameTimings> {
    struct EncodedMsg {
        encoded: EncodedFrame,
        capture_us: u64,
        encode_us: u64,
    }

    let (tx, rx) = mpsc::sync_channel::<EncodedMsg>(2);

    let deadline = Instant::now() + Duration::from_secs(num_seconds);

    // Producer: capture + encode
    let producer = thread::spawn(move || {
        let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");
        let mut encoder = FfmpegEncoder::new(quality).expect("Failed to create encoder");

        println!("  Encoder: {}", encoder.encoder_name());

        while Instant::now() < deadline {
            let t = Instant::now();
            let frame = match capturer.capture() {
                Ok(f) => f,
                Err(_) => continue,
            };
            let capture_us = t.elapsed().as_micros() as u64;

            let t = Instant::now();
            let encoded_frames = match encoder.encode(&frame) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let encode_us = t.elapsed().as_micros() as u64;

            // try_send: drop frame if consumer is behind
            for encoded in encoded_frames {
                let _ = tx.try_send(EncodedMsg {
                    encoded,
                    capture_us,
                    encode_us,
                });
            }
        }
    });

    // Consumer: network + decode + rescale
    let mut decoder = FfmpegDecoder::new().expect("Failed to create decoder");
    let mut rescale_buf: Vec<u8> = Vec::new();
    let mut timings = Vec::new();

    while Instant::now() < deadline {
        let msg = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let frame_bytes = msg.encoded.data.len();
        let is_keyframe = msg.encoded.is_keyframe;

        let network_us = simulate_network(&msg.encoded, net_latency_ms, net_bandwidth);

        let t = Instant::now();
        let decoded = match decoder.decode(&msg.encoded) {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(_) => continue,
        };
        let decode_us = t.elapsed().as_micros() as u64;

        let t = Instant::now();
        rescale_nearest(
            &decoded.data,
            decoded.width,
            decoded.height,
            &mut rescale_buf,
            viewer_w,
            viewer_h,
        );
        let rescale_us = t.elapsed().as_micros() as u64;

        decoder.return_buffer(decoded.data);

        timings.push(FrameTimings {
            capture_us: msg.capture_us,
            encode_us: msg.encode_us,
            network_us,
            decode_us,
            rescale_us,
            frame_bytes,
            is_keyframe,
        });
    }

    let _ = producer.join();
    timings
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn print_report(label: &str, timings: &[FrameTimings], seconds: u64) {
    if timings.is_empty() {
        println!("  No frames captured");
        return;
    }

    let n = timings.len() as u64;

    let avg = |f: fn(&FrameTimings) -> u64| -> f64 {
        timings.iter().map(f).sum::<u64>() as f64 / n as f64
    };

    let p95 = |f: fn(&FrameTimings) -> u64| -> u64 {
        let mut vals: Vec<u64> = timings.iter().map(f).collect();
        vals.sort_unstable();
        vals[(vals.len() as f64 * 0.95) as usize]
    };

    let max = |f: fn(&FrameTimings) -> u64| -> u64 { timings.iter().map(f).max().unwrap_or(0) };

    let total_bytes: usize = timings.iter().map(|t| t.frame_bytes).sum();
    let keyframes: usize = timings.iter().filter(|t| t.is_keyframe).count();

    let avg_total_us =
        avg(|t| t.capture_us + t.encode_us + t.network_us + t.decode_us + t.rescale_us);

    println!(
        "\n=== {label} — {n} frames in {seconds}s ({:.1} fps) ===",
        n as f64 / seconds as f64
    );
    println!();
    println!("  Stage          Avg         P95         Max");
    println!("  ─────────────────────────────────────────────");
    println!(
        "  Capture     {:>7.1}ms   {:>7.1}ms   {:>7.1}ms",
        avg(|t| t.capture_us) / 1000.0,
        p95(|t| t.capture_us) as f64 / 1000.0,
        max(|t| t.capture_us) as f64 / 1000.0,
    );
    println!(
        "  Encode      {:>7.1}ms   {:>7.1}ms   {:>7.1}ms",
        avg(|t| t.encode_us) / 1000.0,
        p95(|t| t.encode_us) as f64 / 1000.0,
        max(|t| t.encode_us) as f64 / 1000.0,
    );
    println!(
        "  Network     {:>7.1}ms   {:>7.1}ms   {:>7.1}ms",
        avg(|t| t.network_us) / 1000.0,
        p95(|t| t.network_us) as f64 / 1000.0,
        max(|t| t.network_us) as f64 / 1000.0,
    );
    println!(
        "  Decode      {:>7.1}ms   {:>7.1}ms   {:>7.1}ms",
        avg(|t| t.decode_us) / 1000.0,
        p95(|t| t.decode_us) as f64 / 1000.0,
        max(|t| t.decode_us) as f64 / 1000.0,
    );
    println!(
        "  Rescale     {:>7.1}ms   {:>7.1}ms   {:>7.1}ms",
        avg(|t| t.rescale_us) / 1000.0,
        p95(|t| t.rescale_us) as f64 / 1000.0,
        max(|t| t.rescale_us) as f64 / 1000.0,
    );
    println!("  ─────────────────────────────────────────────");
    println!(
        "  Total       {:>7.1}ms pipeline per frame",
        avg_total_us / 1000.0
    );
    println!("  Theoretical max FPS: {:.1}", 1_000_000.0 / avg_total_us);
    println!();
    println!(
        "  Bitrate:    {:.2} Mbps",
        total_bytes as f64 * 8.0 / seconds as f64 / 1_000_000.0,
    );
    println!(
        "  Avg frame:  {:.1} KB ({} keyframes)",
        total_bytes as f64 / n as f64 / 1024.0,
        keyframes,
    );

    // Identify bottleneck
    let stages = [
        ("Capture", avg(|t| t.capture_us)),
        ("Encode", avg(|t| t.encode_us)),
        ("Network", avg(|t| t.network_us)),
        ("Decode", avg(|t| t.decode_us)),
        ("Rescale", avg(|t| t.rescale_us)),
    ];

    let bottleneck = stages
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap();

    println!();
    println!(
        "  ⚠ Bottleneck: {} ({:.1}ms avg, {:.0}% of pipeline)",
        bottleneck.0,
        bottleneck.1 / 1000.0,
        bottleneck.1 / avg_total_us * 100.0,
    );
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() == 2 {
        Some((parts[0].parse().ok()?, parts[1].parse().ok()?))
    } else {
        None
    }
}

fn main() {
    env_logger::init();

    let quality = match std::env::args().nth(1).as_deref() {
        Some("hd30") => Quality::HD30,
        Some("hd60") => Quality::HD60,
        Some("fhd30") => Quality::FHD30,
        Some("fhd60") => Quality::FHD60,
        _ => {
            println!("Usage: pipeline_bench [hd30|hd60|fhd30|fhd60] [seconds] [viewer_WxH]");
            println!("Defaulting to HD30, 5s, 1920x1080 viewer");
            Quality::HD30
        }
    };

    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let (viewer_w, viewer_h) = std::env::args()
        .nth(3)
        .and_then(|s| parse_resolution(&s))
        .unwrap_or((1920, 1080));

    // Simulated network: 2ms relay latency, 10 Mbps bandwidth
    let net_latency_ms = 2;
    let net_bandwidth = 10_000_000 / 8; // 10 Mbps → bytes/sec

    println!("╔══════════════════════════════════════════════════╗");
    println!("║  Pipeline Benchmark: {quality}                ");
    println!("║  Duration:   {seconds}s                              ");
    println!("║  Viewer:     {viewer_w}x{viewer_h}                     ");
    println!("║  Network:    {net_latency_ms}ms latency, 10 Mbps             ");
    println!("╚══════════════════════════════════════════════════╝");

    // --- Serial (current architecture) ---
    println!("\n▶ Running SERIAL pipeline (single thread)...");
    let serial = run_serial(
        quality,
        seconds,
        viewer_w,
        viewer_h,
        net_latency_ms,
        net_bandwidth,
    );
    print_report("SERIAL (single thread)", &serial, seconds);

    // --- Threaded (proposed architecture) ---
    println!("\n▶ Running THREADED pipeline (capture+encode ‖ decode+rescale)...");
    let threaded = run_threaded(
        quality,
        seconds,
        viewer_w,
        viewer_h,
        net_latency_ms,
        net_bandwidth,
    );
    print_report("THREADED (capture‖decode)", &threaded, seconds);

    // --- Comparison ---
    if !serial.is_empty() && !threaded.is_empty() {
        let serial_fps = serial.len() as f64 / seconds as f64;
        let threaded_fps = threaded.len() as f64 / seconds as f64;
        let improvement = (threaded_fps - serial_fps) / serial_fps * 100.0;

        println!("\n═══ Comparison ═══");
        println!("  Serial:    {:.1} fps", serial_fps);
        println!("  Threaded:  {:.1} fps", threaded_fps);
        println!(
            "  Δ          {:+.1} fps ({:+.0}%)",
            threaded_fps - serial_fps,
            improvement,
        );
    }
}
