// examples/latency_profile.rs
//
// Run with: `cargo run --example latency_profile`
//
// Measures ACTUAL pipeline latency by detecting impulse propagation:
// when pipeline_level spikes (input detected sound), start a timer.
// When output_level spikes (output played it back via loopback), stop the timer.
// The delta is the real, measured pipeline latency — no human reaction time.
//
// How to use:
// 1. Run the example (it enables loopback automatically)
// 2. Make a sharp loud noise (snap fingers, clap, tap the mic)
// 3. The profiler detects the impulse traveling through the pipeline
//    and prints the measured latency
// 4. Wait 1–2 seconds between snaps for clean measurements

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use core::{AudioStats, VoiceChat, VoiceChatConfig};

// ── Tuning constants ────────────────────────────────────────────────────

/// Minimum level to consider an "impulse" on the input side.
/// High enough to ignore background noise, low enough to catch a clap/snap.
const INPUT_IMPULSE_THRESHOLD: f32 = 0.15;

/// Minimum level to consider the impulse arrived at output.
/// Lower than input because loopback + mixing + gain may attenuate.
const OUTPUT_IMPULSE_THRESHOLD: f32 = 0.02;

/// Cooldown between measurements to avoid double-triggers.
const MEASUREMENT_COOLDOWN: Duration = Duration::from_millis(1500);

/// How fast we poll the level atomics. 0.5 ms gives ±0.5 ms accuracy.
const POLL_INTERVAL: Duration = Duration::from_micros(500);

/// Maximum time to wait for output impulse after input detection.
/// If exceeded, the measurement is discarded.
const MAX_IMPULSE_WAIT: Duration = Duration::from_millis(500);

/// Interval between periodic status prints.
const STATUS_INTERVAL: Duration = Duration::from_secs(5);

// ── State machine ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum MeasureState {
    /// Waiting for a loud impulse to appear on the input pipeline.
    WaitingForInput,
    /// Input impulse detected — waiting for it to appear on output.
    WaitingForOutput,
}

/// Running statistics over collected latency measurements.
struct LatencyStats {
    samples: Vec<f64>,
}

impl LatencyStats {
    fn new() -> Self {
        Self { samples: Vec::new() }
    }

    fn push(&mut self, latency_ms: f64) {
        self.samples.push(latency_ms);
    }

    fn count(&self) -> usize {
        self.samples.len()
    }

    fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn avg(&self) -> f64 {
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    fn min(&self) -> f64 {
        self.samples.iter().copied().fold(f64::MAX, f64::min)
    }

    fn max(&self) -> f64 {
        self.samples.iter().copied().fold(0.0_f64, f64::max)
    }

    fn p50(&self) -> f64 {
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        if n % 2 == 0 && n >= 2 {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        } else {
            sorted[n / 2]
        }
    }
}

// ── Measurement loop ────────────────────────────────────────────────────

fn run_measurement_loop(stats: &AudioStats) {
    let mut latency = LatencyStats::new();
    let mut state = MeasureState::WaitingForInput;
    let mut input_detected_at = Instant::now();
    let mut input_peak: f32 = 0.0;
    // Allow immediate first measurement.
    let mut last_measurement = Instant::now() - MEASUREMENT_COOLDOWN;

    let mut last_status_print = Instant::now();
    let start = Instant::now();

    loop {
        std::thread::sleep(POLL_INTERVAL);

        let pipeline_lvl = f32::from_bits(stats.pipeline_level.load(Ordering::Relaxed));
        let output_lvl = f32::from_bits(stats.output_level.load(Ordering::Relaxed));

        match state {
            MeasureState::WaitingForInput => {
                if last_measurement.elapsed() < MEASUREMENT_COOLDOWN {
                    continue;
                }
                if pipeline_lvl >= INPUT_IMPULSE_THRESHOLD {
                    input_detected_at = Instant::now();
                    input_peak = pipeline_lvl;
                    state = MeasureState::WaitingForOutput;
                }
            }

            MeasureState::WaitingForOutput => {
                // Track rising input (the impulse may span multiple polls).
                if pipeline_lvl > input_peak {
                    input_peak = pipeline_lvl;
                }

                if output_lvl >= OUTPUT_IMPULSE_THRESHOLD {
                    let latency_ms = input_detected_at.elapsed().as_secs_f64() * 1000.0;
                    latency.push(latency_ms);

                    println!(
                        "  🎯 #{:<3} Latency: {:.1}ms  (in={:.3} out={:.3})  │  \
                         avg={:.1}ms  min={:.1}ms  max={:.1}ms  p50={:.1}ms  [n={}]",
                        latency.count(), latency_ms, input_peak, output_lvl,
                        latency.avg(), latency.min(), latency.max(), latency.p50(),
                        latency.count(),
                    );

                    last_measurement = Instant::now();
                    state = MeasureState::WaitingForInput;
                } else if input_detected_at.elapsed() > MAX_IMPULSE_WAIT {
                    println!(
                        "  ⏱️  Timeout: input impulse (level={input_peak:.3}) \
                         but no output after {}ms — discarded",
                        MAX_IMPULSE_WAIT.as_millis(),
                    );
                    last_measurement = Instant::now();
                    state = MeasureState::WaitingForInput;
                }
            }
        }

        // ── Periodic status ─────────────────────────────────────────
        if last_status_print.elapsed() >= STATUS_INTERVAL {
            last_status_print = Instant::now();
            print_periodic_status(stats, start.elapsed().as_secs(), pipeline_lvl, output_lvl);

            if latency.is_empty() {
                println!("  💡 No measurements yet — try clapping or tapping your mic!");
            }
        }
    }
}

fn print_periodic_status(
    stats: &AudioStats,
    elapsed_secs: u64,
    pipeline_lvl: f32,
    output_lvl: f32,
) {
    let jitter_ms = stats.jitter_buffer_occupancy_ms.load(Ordering::Relaxed);
    let underruns = stats.output_underruns.load(Ordering::Relaxed);
    let overruns = stats.input_overruns.load(Ordering::Relaxed);
    let speaking = stats.vad_active.load(Ordering::Relaxed);
    let gate = stats.gate_active.load(Ordering::Relaxed);

    println!(
        "  ── {elapsed_secs}s │ jitter_buf={jitter_ms}ms │ underruns={underruns} │ \
         overruns={overruns} │ gate={} │ vad={} │ in={pipeline_lvl:.3} │ out={output_lvl:.3} ──",
        if gate { "OPEN" } else { "shut" },
        if speaking { "YES" } else { "no" },
    );
}

// ── Entry point ─────────────────────────────────────────────────────────

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║       Audio Pipeline Latency Profiler v2               ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║ Measures REAL pipeline latency via impulse detection.   ║");
    println!("║                                                        ║");
    println!("║ 1. Make a SHARP loud noise (clap, snap, tap the mic)   ║");
    println!("║ 2. Wait for the measurement to appear                  ║");
    println!("║ 3. Repeat — wait ~2s between each snap                 ║");
    println!("║                                                        ║");
    println!("║ Press Ctrl+C to stop.                                  ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    let config = VoiceChatConfig {
        noise_suppression: true,
        // Very low threshold — don't want the gate to eat our impulse.
        gate_threshold: 0.005,
        ..Default::default()
    };

    let mut vc = VoiceChat::new(config).expect("Failed to create VoiceChat");
    vc.start().expect("Failed to start VoiceChat");
    vc.set_loopback(true);

    let stats = vc.stats().clone();

    println!("Pipeline started. Loopback enabled.\n");
    println!("Waiting for impulses... make a sharp noise!\n");

    run_measurement_loop(&stats);
}