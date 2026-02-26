//! Capture a single screenshot from the primary monitor.
//!
//! ```
//! cargo run --example screenshot
//! ```

use video::{Quality, capture::ScreenCapturer};
use std::time::Instant;

fn main() {
    env_logger::init();

    let quality = Quality::HD30; // just for the resolution target
    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");

    let start = Instant::now();
    let frame = capturer.capture().expect("Capture failed");
    let elapsed = start.elapsed();

    println!(
        "Captured {}x{} frame in {:?} ({} bytes RGBA)",
        frame.width,
        frame.height,
        elapsed,
        frame.data.len(),
    );

    // Save as raw RGBA (can open with GIMP: raw image data, RGBA, width×height)
    std::fs::write("screenshot.rgba", &frame.data).expect("Failed to save");
    println!("Saved to screenshot.rgba");
}
