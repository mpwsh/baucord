// examples/sounds.rs
//
// Run with: `cargo run --example sounds`
//
// Plays notification sounds through the output pipeline to verify
// they work correctly. Starts the audio engine, plays join sound,
// waits, plays leave sound, then exits.

use std::{thread, time::Duration};

use audio::{SoundType, VoiceChat, VoiceChatConfig};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    println!("╔══════════════════════════════════════╗");
    println!("║   Notification Sound Test            ║");
    println!("╠══════════════════════════════════════╣");
    println!("║ Playing join sound, then leave sound ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    let config = VoiceChatConfig::default();

    let mut vc = VoiceChat::new(config).expect("Failed to create VoiceChat");
    vc.start().expect("Failed to start VoiceChat");

    // Give the output worker time to initialize and start filling the ring
    thread::sleep(Duration::from_millis(500));

    println!("▶ Playing Joined sound...");
    vc.play_sound(SoundType::UserJoined);
    thread::sleep(Duration::from_secs(2));

    println!("▶ Playing Leave sound...");
    vc.play_sound(SoundType::UserLeft);
    thread::sleep(Duration::from_secs(2));

    println!("Done.");
}
