// examples/stats.rs
//
// Run with: `cargo run --example stats`
//
// Live stats dashboard for voice-chat-core.
// Demonstrates the full pipeline API: devices, gain, VAD gate, codec stats,
// bandwidth estimation, and buffer health — all wired to real-time controls.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use fltk::{app, enums, frame, group, prelude::*, window};
use log::error;

use audio::{VoiceChat, VoiceChatConfig};
use ui::theme;
use ui::widgets::{controls, labels, layout, meter};

// ── Constants ───────────────────────────────────────────────────────────

/// Interval between stat refreshes (8 Hz).
const STAT_INTERVAL_SECS: f64 = 0.125;

/// Delay before starting the audio engine (lets the window render first).
const ENGINE_START_DELAY_SECS: f64 = 0.05;

// ── Timer data ──────────────────────────────────────────────────────────
//
// These structs exist so the timer closure can receive one bundle
// instead of 15+ individually cloned handles.

/// Shared atomic levels written by the timer, read by meter draw closures.
struct Levels {
    input: Arc<AtomicU32>,
    output: Arc<AtomicU32>,
    denoised: Arc<AtomicU32>,
}

/// VU meter frames that need periodic redraw.
struct MeterWidgets {
    input: frame::Frame,
    output: frame::Frame,
    gate: frame::Frame,
}

/// All the stat-readout labels the timer updates each tick.
struct StatWidgets {
    speaking: frame::Frame,
    opus: frame::Frame,
    bw: frame::Frame,
    timing: frame::Frame,
    state: frame::Frame,
    packets: frame::Frame,
    loss: frame::Frame,
    recovery: frame::Frame,
    buffers: frame::Frame,
    jitter: frame::Frame,
    uptime: frame::Frame,
    raw: frame::Frame,
}

// ── Domain formatting ───────────────────────────────────────────────────

fn format_sensitivity(value: f64) -> String {
    let name = match value {
        v if v < 0.010 => "Very High",
        v if v < 0.020 => "High",
        v if v < 0.040 => "Medium",
        v if v < 0.065 => "Low",
        _ => "Very Low",
    };
    format!("Sensitivity: {name}  (threshold: {value:.3} RMS)")
}

// ── Timer ───────────────────────────────────────────────────────────────

fn start_stat_timer(
    vc: Rc<RefCell<VoiceChat>>,
    levels: Levels,
    mut meters: MeterWidgets,
    mut stats: StatWidgets,
) {
    let prev_sent = RefCell::new(0u64);
    let prev_speak = RefCell::new(false);

    app::add_timeout3(STAT_INTERVAL_SECS, move |handle| {
        let vc_ref = vc.borrow();
        let s = vc_ref.stats();

        // Levels
        levels.input.store(vc_ref.input_level().to_bits(), Ordering::Relaxed);
        levels.output.store(vc_ref.output_level().to_bits(), Ordering::Relaxed);
        levels.denoised.store(vc_ref.denoised_level().to_bits(), Ordering::Relaxed);
        meters.input.redraw();
        meters.output.redraw();
        meters.gate.redraw();

        // Speaking indicator
        let speaking = vc_ref.is_speaking();
        if speaking != *prev_speak.borrow() {
            *prev_speak.borrow_mut() = speaking;
            if speaking {
                stats.speaking.set_label("\u{1f7e2}  SPEAKING \u{2014} transmitting audio");
                stats.speaking.set_label_color(theme::SUCCESS);
            } else {
                stats.speaking.set_label("\u{26ab}  Not transmitting");
                stats.speaking.set_label_color(theme::TEXT_MUTED);
            }
            stats.speaking.redraw();
        }

        // Codec
        let last_pkt = s.last_packet_size.load(Ordering::Relaxed);
        let avg_pkt = f32::from_bits(s.avg_packet_size.load(Ordering::Relaxed));
        stats.opus.set_label(&format!(
            "Opus: {last_pkt}B last, {avg_pkt:.0}B avg  |  24kbps VBR, 10ms frames",
        ));

        let bw_send = s.estimated_send_kbps();
        let bw_recv = s.estimated_recv_kbps();
        let total_sent_kb = s.bytes_sent.load(Ordering::Relaxed) as f32 / 1024.0;
        let total_recv_kb = s.bytes_received.load(Ordering::Relaxed) as f32 / 1024.0;
        stats.bw.set_label(&format!(
            "BW: \u{2191} {bw_send:.1} kbps ({total_sent_kb:.0} KB)  \u{2193} {bw_recv:.1} kbps ({total_recv_kb:.0} KB)",
        ));

        let enc_us = s.last_encode_us.load(Ordering::Relaxed);
        let dec_us = s.last_decode_us.load(Ordering::Relaxed);
        let budget_pct = (enc_us as f32 / 10_000.0) * 100.0;
        stats.timing.set_label(&format!(
            "Encode: {enc_us}\u{00b5}s  Decode: {dec_us}\u{00b5}s  (budget: {budget_pct:.0}% of 10ms)",
        ));

        let dtx = s.dtx_active.load(Ordering::Relaxed);
        let denoise = s.denoise_active.load(Ordering::Relaxed);
        let vad_on = s.vad_active.load(Ordering::Relaxed);
        let gate_on = s.gate_active.load(Ordering::Relaxed);
        stats.state.set_label(&format!(
            "DTX: {}  |  Denoise: {}  |  Gate: {}  |  VAD: {}",
            if dtx { "ON (silence)" } else { "off" },
            if denoise { "ON" } else { "off" },
            if gate_on { "OPEN" } else { "closed" },
            if vad_on { "SPEECH" } else { "quiet" },
        ));

        // Pipeline health
        let sent = s.packets_sent.load(Ordering::Relaxed);
        let recv = s.packets_received.load(Ordering::Relaxed);
        let lost = s.packets_lost.load(Ordering::Relaxed);

        let prev = *prev_sent.borrow();
        let delta = sent.saturating_sub(prev);
        *prev_sent.borrow_mut() = sent;
        let pps = delta as f32 * 8.0;

        let pps_note = if pps < 0.1 && sent > 0 { "  (idle: VAD gating)" } else { "" };
        stats.packets.set_label(&format!(
            "Packets: sent {sent} recv {recv} lost {lost}  |  {pps:.0} pkt/s{pps_note}",
        ));

        let loss_pct = if recv + lost > 0 {
            (lost as f32 / (recv + lost) as f32) * 100.0
        } else {
            0.0
        };
        stats.loss.set_label(&format!("Loss rate: {loss_pct:.1}%"));

        let fec = s.fec_recoveries.load(Ordering::Relaxed);
        let plc = s.plc_frames.load(Ordering::Relaxed);
        stats.recovery.set_label(&format!("FEC recoveries: {fec}  PLC frames: {plc}"));

        let overruns = s.input_overruns.load(Ordering::Relaxed);
        let underruns = s.output_underruns.load(Ordering::Relaxed);
        stats.buffers.set_label(&format!("Overruns: {overruns}  Underruns: {underruns}"));

        let jitter_ms = s.jitter_buffer_occupancy_ms.load(Ordering::Relaxed);
        stats.jitter.set_label(&format!("Jitter buffer: {jitter_ms} ms"));

        let uptime = s.start_time.elapsed().as_secs();
        stats.uptime.set_label(&format!("Uptime: {}m {}s", uptime / 60, uptime % 60));

        stats.raw.set_label(&vc_ref.stats_summary());

        app::repeat_timeout3(STAT_INTERVAL_SECS, handle);
    });
}

// ── Entry point ─────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Init FLTK first (before any audio threads — avoids COM/Win32 conflicts on Windows).
    let app_handle = app::App::default();
    theme::apply();

    let config = VoiceChatConfig::default();
    let vc = Rc::new(RefCell::new(VoiceChat::new(config)?));

    let input_devices = VoiceChat::list_input_devices()
        .inspect_err(|e| error!("Failed to list input devices: {e}"))
        .unwrap_or_default();
    let output_devices = VoiceChat::list_output_devices()
        .inspect_err(|e| error!("Failed to list output devices: {e}"))
        .unwrap_or_default();

    // ── Window ──────────────────────────────────────────────────────

    let mut wind = window::Window::default()
        .with_size(620, 860)
        .with_label("voice-chat-core \u{2014} Stats Dashboard");
    wind.make_resizable(false);

    let mut scroll = group::Scroll::default_fill();
    scroll.set_scrollbar_size(8);

    let mut col = group::Flex::default()
        .with_size(590, 1180)
        .with_pos(10, 10)
        .column();
    col.set_spacing(4);

    // ── Devices ─────────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f3a7}", "DEVICES");

    labels::field_label(&mut col, "Input Device");
    let mut input_choice = controls::dropdown(&mut col);
    let input_names: Vec<&str> = input_devices.iter().map(|(n, _)| n.as_str()).collect();
    controls::populate_dropdown(&mut input_choice, "System Default", &input_names, None);

    labels::field_label(&mut col, "Output Device");
    let mut output_choice = controls::dropdown(&mut col);
    let output_names: Vec<&str> = output_devices.iter().map(|(n, _)| n.as_str()).collect();
    controls::populate_dropdown(&mut output_choice, "System Default", &output_names, None);

    labels::hint_label(
        &mut col,
        &format!(
            "{} input, {} output devices found",
            input_devices.len(),
            output_devices.len()
        ),
    );
    layout::separator(&mut col);

    // ── Input ───────────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f3a4}", "INPUT");

    labels::field_label(&mut col, "Capture Level (pre-denoise)");
    let input_level = Arc::new(AtomicU32::new(0));
    let vu_input = meter::vu(&mut col, Arc::clone(&input_level), 22, theme::SUCCESS);

    labels::field_label(&mut col, "Input Gain");
    let mut input_gain_slider =
        controls::slider(&mut col, 0.0, 2.0, 1.0, 0.05, enums::Color::Selection);
    let mut input_gain_lbl = labels::hint_label(&mut col, "Gain: 100%");

    layout::separator(&mut col);

    // ── Output ──────────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f50a}", "OUTPUT");

    let output_level = Arc::new(AtomicU32::new(0));
    let vu_output = meter::vu(&mut col, Arc::clone(&output_level), 22, theme::CYAN);

    labels::field_label(&mut col, "Master Volume");
    let mut output_gain_slider =
        controls::slider(&mut col, 0.0, 2.0, 1.0, 0.05, theme::CYAN);
    let mut output_gain_lbl = labels::hint_label(&mut col, "Volume: 100%");

    layout::separator(&mut col);

    // ── Voice Gate ──────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{26a1}", "VOICE GATE");

    let mut speaking_lbl = frame::Frame::default()
        .with_label("\u{26ab}  Not transmitting")
        .with_align(enums::Align::Left | enums::Align::Inside);
    speaking_lbl.set_label_color(theme::TEXT_MUTED);
    speaking_lbl.set_label_size(12);
    col.fixed(&speaking_lbl, 22);

    labels::field_label(&mut col, "Denoised Level vs Threshold");
    let denoised_level = Arc::new(AtomicU32::new(0));
    let threshold: Rc<RefCell<f32>> = Rc::new(RefCell::new(0.020));
    let vu_gate = meter::gate(
        &mut col,
        Arc::clone(&denoised_level),
        Rc::clone(&threshold),
        28,
    );

    labels::field_label(&mut col, "Gate Threshold");
    let mut vad_slider =
        controls::slider(&mut col, 0.005, 0.10, 0.020, 0.001, enums::Color::Selection);
    let mut vad_sens_lbl = labels::hint_label(&mut col, &format_sensitivity(0.020));

    let noise_toggle = controls::checkbox(&mut col, "Noise suppression (RNNoise)", true);
    let mute_toggle = controls::checkbox(&mut col, "Mute microphone", false);
    let loopback_toggle = controls::checkbox(&mut col, "Monitor input (loopback)", false);

    layout::separator(&mut col);

    // ── Codec & Bandwidth ───────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f4ca}", "CODEC & BANDWIDTH");

    let stat_opus = labels::stat_label(&mut col, "Opus: --");
    let stat_bw = labels::stat_label(&mut col, "Bandwidth: --");
    let stat_timing = labels::stat_label(&mut col, "Timing: --");
    let stat_state = labels::stat_label(&mut col, "State: --");

    layout::separator(&mut col);

    // ── Pipeline Health ─────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1fa7a}", "PIPELINE HEALTH");

    let stat_packets = labels::stat_label(&mut col, "Packets: --");
    let stat_loss = labels::stat_label(&mut col, "Loss: --");
    let stat_recovery = labels::stat_label(&mut col, "Recovery: --");
    let stat_buffers = labels::stat_label(&mut col, "Buffers: --");
    let stat_jitter = labels::stat_label(&mut col, "Jitter buffer: --");
    let stat_uptime = labels::stat_label(&mut col, "Uptime: --");

    layout::separator(&mut col);

    // ── Raw Summary ─────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f4cb}", "RAW SUMMARY");
    let mut stat_raw = frame::Frame::default()
        .with_label("...")
        .with_align(enums::Align::Left | enums::Align::Inside | enums::Align::Wrap);
    stat_raw.set_label_color(theme::TEXT_MUTED);
    stat_raw.set_label_size(9);
    col.fixed(&stat_raw, 42);

    col.end();
    scroll.end();
    wind.end();
    wind.show();

    // ── Deferred engine start ───────────────────────────────────────

    let vc_start = Rc::clone(&vc);
    app::add_timeout3(ENGINE_START_DELAY_SECS, move |_| {
        if let Err(e) = vc_start.borrow_mut().start() {
            error!("Failed to start voice engine: {e}");
        }
    });

    // ── Callbacks ───────────────────────────────────────────────────

    let input_devs = input_devices.clone();
    let vc_c = Rc::clone(&vc);
    input_choice.set_callback(move |c| {
        let idx = c.value() as usize;
        let dev = if idx == 0 { None } else { input_devs.get(idx - 1).map(|(n, _)| n.as_str()) };
        if let Err(e) = vc_c.borrow_mut().set_input_device(dev) {
            error!("Input device error: {e}");
        }
    });

    let output_devs = output_devices.clone();
    let vc_c = Rc::clone(&vc);
    output_choice.set_callback(move |c| {
        let idx = c.value() as usize;
        let dev = if idx == 0 { None } else { output_devs.get(idx - 1).map(|(n, _)| n.as_str()) };
        if let Err(e) = vc_c.borrow_mut().set_output_device(dev) {
            error!("Output device error: {e}");
        }
    });

    let vc_c = Rc::clone(&vc);
    input_gain_slider.set_callback(move |s| {
        let val = s.value() as f32;
        vc_c.borrow().set_input_gain(val);
        input_gain_lbl.set_label(&format!("Gain: {}%", (val * 100.0) as i32));
    });

    let vc_c = Rc::clone(&vc);
    output_gain_slider.set_callback(move |s| {
        let val = s.value() as f32;
        vc_c.borrow().set_output_gain(val);
        output_gain_lbl.set_label(&format!("Volume: {}%", (val * 100.0) as i32));
    });

    let threshold_cb = Rc::clone(&threshold);
    let vc_c = Rc::clone(&vc);
    let mut vu_gate_cb = vu_gate.clone();
    vad_slider.set_callback(move |s| {
        let val = s.value() as f32;
        *threshold_cb.borrow_mut() = val;
        vad_sens_lbl.set_label(&format_sensitivity(s.value()));
        vu_gate_cb.redraw();
        vc_c.borrow().set_gate_threshold(val);
    });

    // FLTK: clone required — set_callback takes ownership via move closure.
    let vc_c = Rc::clone(&vc);
    noise_toggle.clone().set_callback(move |b| {
        vc_c.borrow().set_noise_suppression(b.is_checked());
    });
    let vc_c = Rc::clone(&vc);
    mute_toggle.clone().set_callback(move |b| {
        vc_c.borrow().set_muted(b.is_checked());
    });
    let vc_c = Rc::clone(&vc);
    loopback_toggle.clone().set_callback(move |b| {
        vc_c.borrow().set_loopback(b.is_checked());
    });

    wind.set_callback(|w| {
        if app::event() == enums::Event::Close {
            w.hide();
        }
    });

    // ── Stat timer ──────────────────────────────────────────────────

    let levels = Levels {
        input: input_level,
        output: output_level,
        denoised: denoised_level,
    };
    let meters = MeterWidgets {
        input: vu_input,
        output: vu_output,
        gate: vu_gate,
    };
    let stat_widgets = StatWidgets {
        speaking: speaking_lbl,
        opus: stat_opus,
        bw: stat_bw,
        timing: stat_timing,
        state: stat_state,
        packets: stat_packets,
        loss: stat_loss,
        recovery: stat_recovery,
        buffers: stat_buffers,
        jitter: stat_jitter,
        uptime: stat_uptime,
        raw: stat_raw,
    };

    start_stat_timer(Rc::clone(&vc), levels, meters, stat_widgets);

    app_handle.run()?;
    Ok(())
}