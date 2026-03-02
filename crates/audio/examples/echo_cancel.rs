// examples/echo_cancel.rs
//
// Run with: `cargo run --example echo_cancel`
//
// Live echo cancellation test. Starts the full voice pipeline (mic capture +
// output) and system audio capture simultaneously. Speak into your mic while
// it records, then compare the output WAV files.
//
// What happens:
//   1. Voice pipeline starts (mic → denoise → gate → encode → decode → output)
//   2. Loopback is enabled so your voice plays through the speakers
//   3. Two system audio captures run in parallel:
//      - One WITH echo cancellation (voice removed from loopback)
//      - One WITHOUT echo cancellation (raw loopback for comparison)
//   4. After recording, packets are decoded and written to WAV files
//
// Output files:
//   echo_cancelled.wav  — system capture WITH echo cancellation (voice removed)
//   echo_raw.wav        — system capture WITHOUT echo cancellation (voice present)
//
// Tips:
//   - Play music or a YouTube video while talking for the clearest comparison
//   - The raw file will have your voice + music, the cancelled file should
//     have mostly just the music with your voice attenuated
//   - Works best when your voice is clearly audible in the loopback

use audio::stream::input::SystemAudioCapture;
use audio::{AudioStats, VoiceChat, VoiceChatConfig};

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 48000;
const RECORD_SECONDS: u32 = 10;

/// Write a stereo WAV file (48kHz, 16-bit, interleaved).
fn write_wav_stereo(path: &str, samples: &[f32]) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

fn decode_packets(
    packets: &[audio::stream::input::SystemAudioPacket],
) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?;
    let mut pcm = Vec::new();
    let mut decode_buf = vec![0.0f32; 960 * 2]; // 20ms stereo interleaved

    for pkt in packets {
        match decoder.decode_float(&pkt.data, &mut decode_buf, false) {
            Ok(samples_per_ch) => {
                pcm.extend_from_slice(&decode_buf[..samples_per_ch * 2]);
            }
            Err(e) => log::warn!("Decode error: {e}"),
        }
    }

    Ok(pcm)
}

fn write_results(
    ec_packets: &[audio::stream::input::SystemAudioPacket],
    raw_packets: &[audio::stream::input::SystemAudioPacket],
) -> Result<(), Box<dyn std::error::Error>> {
    // ── Decode packets to PCM ───────────────────────────────────────
    println!("Decoding Opus packets to PCM...");

    let ec_pcm = decode_packets(ec_packets)?;
    let raw_pcm = decode_packets(raw_packets)?;

    let ec_duration = ec_pcm.len() as f32 / (SAMPLE_RATE as f32 * 2.0);
    let raw_duration = raw_pcm.len() as f32 / (SAMPLE_RATE as f32 * 2.0);
    println!(
        "  EC:  {:.2}s  ({} interleaved samples)",
        ec_duration,
        ec_pcm.len()
    );
    println!(
        "  Raw: {:.2}s  ({} interleaved samples)",
        raw_duration,
        raw_pcm.len()
    );

    // ── Quality metrics ─────────────────────────────────────────────
    let ec_rms = rms(&ec_pcm);
    let raw_rms = rms(&raw_pcm);

    println!();
    println!("RMS levels:");
    println!("  Echo-cancelled:    {:.4}", ec_rms);
    println!("  Raw (with voice):  {:.4}", raw_rms);

    if ec_rms > 0.0 && raw_rms > 0.0 {
        let ratio_db = 20.0 * (ec_rms / raw_rms).log10();
        println!("  Ratio: {:.1} dB", ratio_db);
        if ratio_db < -1.0 {
            println!("  → Voice energy was reduced (good!)");
        } else if ratio_db < 1.0 {
            println!(
                "  → Similar levels — try speaking louder or increasing loopback volume"
            );
        } else {
            println!("  → EC output is louder — unexpected, check delay estimation");
        }
    }

    // ── Write WAV files ─────────────────────────────────────────────
    println!();
    println!("Writing WAV files...");

    if !ec_pcm.is_empty() {
        write_wav_stereo("echo_cancelled.wav", &ec_pcm)?;
        println!("  ✅ echo_cancelled.wav  ({:.1}s, stereo)", ec_duration);
    } else {
        println!("  ⚠️  No echo-cancelled audio to write");
    }

    if !raw_pcm.is_empty() {
        write_wav_stereo("echo_raw.wav", &raw_pcm)?;
        println!("  ✅ echo_raw.wav        ({:.1}s, stereo)", raw_duration);
    } else {
        println!("  ⚠️  No raw audio to write");
    }

    println!();
    println!("🎧 Compare the files:");
    println!("  echo_raw.wav        → your voice + background audio (no cancellation)");
    println!("  echo_cancelled.wav  → background audio with voice removed/reduced");
    println!();
    println!("If both sound the same, the delay estimator may not have converged.");
    println!("Try again with louder speech or longer recording duration.");

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔══════════════════════════════════════════╗");
    println!("║   Live Echo Cancellation Test            ║");
    println!("╠══════════════════════════════════════════╣");
    println!("║  Duration: {RECORD_SECONDS}s recording                  ║");
    println!("║  Talk into your mic while this runs.     ║");
    println!("║  Play music/game audio for best results. ║");
    println!("╚══════════════════════════════════════════╝");
    println!();

    // Collect packets here so they outlive the voice engine.
    let mut ec_packets = Vec::new();
    let mut raw_packets = Vec::new();

    {
        // ── Start the voice chat engine ─────────────────────────────
        // Everything in this block is dropped at the end, which shuts
        // down the engine cleanly. The captures are stopped explicitly
        // BEFORE the voice engine drops, so the encoder channel
        // disconnects correctly.
        let config = VoiceChatConfig::default();
        let mut vc = VoiceChat::new(config)?;

        // Enable loopback so our voice plays through the output device.
        // This means the system loopback capture will contain our voice,
        // giving the echo canceller something to remove.
        vc.set_loopback(true);

        vc.start()?;
        println!("[OK] Voice pipeline started (mic → output with loopback)");

        // Give the audio devices a moment to stabilize
        std::thread::sleep(Duration::from_millis(200));

        // ── Start system audio capture WITH echo cancellation ───────
        let stats_ec = Arc::new(AudioStats::new());
        let voice_ref = vc.voice_reference();

        let mut capture_ec = SystemAudioCapture::new(
            stats_ec.clone(),
            Some(voice_ref), // echo cancellation ON
        )?;
        capture_ec.start()?;
        println!("[OK] System capture started (WITH echo cancellation)");

        // ── Start system audio capture WITHOUT echo cancellation ────
        let stats_raw = Arc::new(AudioStats::new());
        let mut capture_raw = SystemAudioCapture::new(
            stats_raw.clone(),
            None, // echo cancellation OFF
        )?;
        capture_raw.start()?;
        println!("[OK] System capture started (WITHOUT echo cancellation)");

        // ── Record for the specified duration ───────────────────────
        println!();
        println!("🎙️  Recording... speak into your mic!");
        println!("    (play music in the background for best demo)");
        println!();

        let start = Instant::now();
        let duration = Duration::from_secs(RECORD_SECONDS as u64);

        while start.elapsed() < duration {
            // Collect echo-cancelled packets
            while let Some(pkt) = capture_ec.try_recv() {
                ec_packets.push(pkt);
            }

            // Collect raw packets
            while let Some(pkt) = capture_raw.try_recv() {
                raw_packets.push(pkt);
            }

            // Progress indicator
            let elapsed = start.elapsed().as_secs();
            let remaining = RECORD_SECONDS as u64 - elapsed.min(RECORD_SECONDS as u64);
            print!(
                "\r  ⏱  {}s remaining  |  ec: {} pkts  |  raw: {} pkts    ",
                remaining,
                ec_packets.len(),
                raw_packets.len(),
            );
            std::io::stdout().flush().ok();

            std::thread::sleep(Duration::from_millis(10));
        }

        // Drain any remaining packets
        while let Some(pkt) = capture_ec.try_recv() {
            ec_packets.push(pkt);
        }
        while let Some(pkt) = capture_raw.try_recv() {
            raw_packets.push(pkt);
        }

        println!();
        println!();

        // ── Stop captures FIRST ─────────────────────────────────────
        // Critical: stop the system audio captures before the voice
        // engine drops. VoiceChat::drop joins the encoder thread, which
        // blocks on recv() until all senders are dropped. AudioInput's
        // worker thread holds a sender clone — it needs to exit before
        // the encoder channel disconnects. Stopping captures first
        // ensures they don't hold any references that could interfere.
        capture_ec.stop();
        capture_raw.stop();

        println!("[OK] Captures stopped");

        // Mute the input so the worker thread stops sending frames.
        // This helps the encoder drain its channel before shutdown.
        vc.set_muted(true);

        // Brief pause so the input worker's last frames drain through
        // the encoder before we drop everything.
        std::thread::sleep(Duration::from_millis(50));

        println!("[OK] Shutting down voice engine...");

        // `vc`, `capture_ec`, `capture_raw` all drop here.
        // VoiceChat::drop sets should_stop, drops input/output devices,
        // replaces channel senders with dead ones, then joins threads.
    }

    println!("[OK] Engine stopped");
    println!();

    // ── Process results (engine is fully shut down) ─────────────────
    println!("  Echo-cancelled packets: {}", ec_packets.len());
    println!("  Raw packets:            {}", raw_packets.len());
    println!();

    if ec_packets.is_empty() && raw_packets.is_empty() {
        println!("⚠️  No packets captured! Possible causes:");
        println!("  - System audio capture not supported on this platform");
        println!("  - No audio was playing through the default output device");
        println!("  - WASAPI loopback failed to start (check logs above)");
        return Ok(());
    }

    write_results(&ec_packets, &raw_packets)?;

    Ok(())
}