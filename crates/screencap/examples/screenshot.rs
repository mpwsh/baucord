//! Capture a single screenshot from the primary monitor.
//!
//! ```
//! cargo run --example screenshot
//! ```

use screencap::{capture::ScreenCapturer, Quality};
use std::time::Instant;

fn main() {
    env_logger::init();

    // macOS requires screen recording permission via ScreenCaptureKit.
    if !screencap::capture::is_supported() {
        eprintln!("Platform not supported");
        std::process::exit(1);
    }
    if !screencap::capture::has_permission() {
        eprintln!("Screen recording permission not granted — requesting...");
        if !screencap::capture::request_permission() {
            eprintln!("Permission denied. Grant access in System Settings → Privacy → Screen Recording, then restart.");
            std::process::exit(1);
        }
    }

    let quality = Quality::HD30; // just for the resolution target
    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");

    let start = Instant::now();
    let frame = capturer.capture().expect("Capture failed");
    let elapsed = start.elapsed();

    println!(
        "Captured {}x{} frame in {:?} ({} bytes BGRA)",
        frame.width,
        frame.height,
        elapsed,
        frame.data.len(),
    );

    // Save as raw BGRA (can open with GIMP: raw image data, BGRA, width×height)
    std::fs::write("screenshot.bgra", &frame.data).expect("Failed to save");
    println!("Saved to screenshot.bgra");
}
