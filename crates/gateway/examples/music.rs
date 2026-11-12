//! A music bot with nothing to install: one binary that is the bot
//! and its own gateway (`gateway::bot::Bot`), decoding files and URLs
//! in-process — symphonia for mp3, aac/m4a, flac, ogg/vorbis and wav,
//! rubato to 48 kHz mono, the same crates the audio engine uses — and
//! playing them in the asker's voice channel.
//!
//! ```text
//! cargo run -p gateway --example music -- --server play.example.com:51000 \
//!     --name MusicBot --key musicbot.key --invite ABCD-EFGH-IJKL --library ~/Music
//! ```
//!
//! In chat: `!play <file or url>` (queues when something plays),
//! `!skip`, `!stop`, `!queue`, `!help`. Files are looked up under
//! `--library` and never outside it; URLs are fetched as they play
//! (an internet radio stream plays until `!stop`). No `YouTube`: that
//! needs an extractor that breaks monthly, which is `yt-dlp`'s job in
//! `music_bot.py`.

use std::collections::{HashMap, VecDeque};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::Parser;
use crossbeam_channel::{Receiver, Sender};
use gateway::bot::{Actor, Bot};
use gateway::net::Config;
use gateway::voice::Feeder;
use gateway::wire::{Action, Outbound};
use log::{error, info, warn};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use serde_json::Value;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use transport::{ClientKey, Fingerprint};

#[derive(Parser, Debug)]
#[command(name = "music", version, about = "A music bot for a baucord server.")]
struct Args {
    /// The server, as `host:port`.
    #[arg(long, env = "BAUCORD_SERVER")]
    server: String,
    /// The server's identity, `SHA256:…`; learned on first use otherwise.
    #[arg(long, env = "BAUCORD_PIN")]
    pin: Option<String>,
    /// The bot's name on the server.
    #[arg(long, env = "BAUCORD_BOT_NAME", default_value = "MusicBot")]
    name: String,
    /// The bot's identity key; made on the first run.
    #[arg(long, env = "BAUCORD_KEY", default_value = "musicbot.key")]
    key: PathBuf,
    /// An invite code, needed on the first run.
    #[arg(long, env = "BAUCORD_INVITE")]
    invite: Option<String>,
    /// The folder `!play <file>` may read from. Without it, URLs only.
    #[arg(long, env = "BAUCORD_LIBRARY")]
    library: Option<PathBuf>,
    /// Opus bitrate for what the bot plays, in kbit/s (16–128).
    #[arg(long, env = "BAUCORD_VOICE_KBPS", default_value_t = 64)]
    voice_kbps: u32,
}

/// Where a track comes from.
#[derive(Debug, Clone)]
enum Source {
    File(PathBuf),
    Url(String),
}

/// One `!play`.
#[derive(Debug, Clone)]
struct Request {
    source: Source,
    label: String,
    asker: String,
}

/// What the chat thread tells the player, in order — so a `!stop`
/// forgets the requests that came before it and none that come after,
/// and a `!skip` while nothing plays is nothing.
enum Control {
    Play(Request),
    Skip,
    Stop,
}

/// What the chat thread may ask of the player. The bot says little in
/// chat — "Now playing", a queue position, an error — because the
/// server's chat limit (5 lines in 5 s by default) applies to it as to
/// anyone, and a chatty bot loses the lines that matter.
struct Player {
    control: Sender<Control>,
    shared: Arc<Shared>,
}

/// What the player's thread lets the chat thread see.
#[derive(Default)]
struct Shared {
    /// A track is playing (decoding, or queued in the voice sink).
    playing: AtomicBool,
    /// Requests waiting behind it.
    queued: AtomicUsize,
}

impl Player {
    fn start(bot: &Bot) -> Self {
        let (control, inbox) = crossbeam_channel::unbounded::<Control>();
        let shared = Arc::new(Shared::default());
        let worker = Worker {
            control: inbox,
            queue: VecDeque::new(),
            shared: Arc::clone(&shared),
            feeder: bot.voice(),
            stopper: bot.voice(),
            actor: bot.actor(),
        };
        thread::Builder::new()
            .name("player".to_string())
            .spawn(move || worker.run())
            .expect("a thread");
        Self { control, shared }
    }

    /// Queue a request; its place in the line when something is
    /// playing, `None` when it plays now.
    fn play(&self, request: Request) -> Option<usize> {
        let playing = self.shared.playing.load(Ordering::Relaxed);
        let _ = self.control.send(Control::Play(request));
        playing.then(|| self.shared.queued.load(Ordering::Relaxed) + 1)
    }

    fn skip(&self) {
        let _ = self.control.send(Control::Skip);
    }

    fn stop(&self) {
        let _ = self.control.send(Control::Stop);
    }

    fn queued(&self) -> usize {
        self.shared.queued.load(Ordering::Relaxed)
    }
}

/// The player's thread: one track at a time, decoded at the room's
/// pace (the feeder blocks when ten seconds are queued), done when the
/// room has played the last frame.
struct Worker {
    control: Receiver<Control>,
    queue: VecDeque<Request>,
    shared: Arc<Shared>,
    /// The decoder feeds through this one…
    feeder: Feeder,
    /// …and the control side stops through this one (every handle
    /// shares the sink), so the two closures below borrow apart.
    stopper: Feeder,
    actor: Actor,
}

/// Drain the control channel into the queue. `true` when the track
/// playing now must end (a skip, or a stop — which also empties the
/// queue and the voice sink).
fn poll(
    control: &Receiver<Control>,
    queue: &mut VecDeque<Request>,
    feeder: &mut Feeder,
    shared: &Shared,
) -> bool {
    let mut interrupt = false;
    while let Ok(command) = control.try_recv() {
        match command {
            Control::Play(request) => queue.push_back(request),
            Control::Skip => interrupt = true,
            Control::Stop => {
                queue.clear();
                interrupt = true;
            }
        }
    }
    shared.queued.store(queue.len(), Ordering::Relaxed);
    if interrupt {
        feeder.stop();
    }
    interrupt
}

impl Worker {
    fn run(mut self) {
        loop {
            while self.queue.is_empty() {
                // Idle: a skip or a stop here is nothing to do.
                match self.control.recv() {
                    Ok(Control::Play(request)) => self.queue.push_back(request),
                    Ok(Control::Skip | Control::Stop) => {}
                    Err(_) => return,
                }
            }
            let Some(request) = self.queue.pop_front() else {
                continue;
            };
            self.shared
                .queued
                .store(self.queue.len(), Ordering::Relaxed);
            self.shared.playing.store(true, Ordering::Relaxed);
            self.actor.act(Action::CreateMessage {
                text: format!("Now playing: {} (for {})", request.label, request.asker),
            });
            info!("playing {} for {}", request.label, request.asker);
            let Worker {
                control,
                queue,
                shared,
                feeder,
                stopper,
                ..
            } = &mut self;
            let mut interrupted = false;
            let outcome = decode(
                &request.source,
                &mut || {
                    interrupted = poll(control, queue, stopper, shared);
                    interrupted
                },
                &mut |samples| feeder.feed(samples),
            );
            // Decoding runs ahead of the room by up to ten seconds:
            // the track is over when the room has played it, or when
            // a skip or a stop cuts it.
            while !interrupted && stopper.pending() > 0 {
                thread::sleep(Duration::from_millis(20));
                interrupted = poll(control, queue, stopper, shared);
            }
            self.shared.playing.store(false, Ordering::Relaxed);
            if let Err(e) = outcome {
                warn!("{}: {e}", request.label);
                self.actor.act(Action::CreateMessage {
                    text: format!("Couldn't play {}: {e}", request.label),
                });
            }
        }
    }
}

// ── Decoding ────────────────────────────────────────────────────────────

/// Decode `source` to 48 kHz mono f32, handing runs of samples to
/// `sink` until the track ends, `interrupted` says so, or `sink` says
/// no.
fn decode(
    source: &Source,
    interrupted: &mut dyn FnMut() -> bool,
    sink: &mut dyn FnMut(&[f32]) -> bool,
) -> Result<(), String> {
    let (mss, hint) = open(source)?;
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("not a format I can read ({e})"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no audio track")?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
        return Err("no audio codec parameters".to_string());
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| format!("no decoder for this codec ({e})"))?;
    let mut to_48k: Option<Mono48k> = None;
    let mut interleaved: Vec<f32> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    loop {
        if interrupted() {
            return Ok(());
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(format!("reading the stream: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            Err(SymphoniaError::DecodeError(e)) => {
                warn!("a packet did not decode ({e}); skipped");
                continue;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(format!("decoding: {e}")),
        };
        let (rate, channels) = downmix(&buffer, &mut interleaved, &mut mono);
        if channels == 0 {
            continue;
        }
        let out: &[f32] = if rate == 48_000 {
            &mono
        } else {
            let resampler = match &mut to_48k {
                Some(r) if r.rate == rate => r,
                _ => to_48k.insert(Mono48k::new(rate)?),
            };
            resampler.push(&mono)
        };
        if !out.is_empty() && !sink(out) {
            return Err("the session is gone".to_string());
        }
    }
    Ok(())
}

/// The decoded buffer as interleaved f32, then averaged to mono.
/// Returns (sample rate, channels).
fn downmix(
    buffer: &GenericAudioBufferRef<'_>,
    interleaved: &mut Vec<f32>,
    mono: &mut Vec<f32>,
) -> (u32, usize) {
    let spec = buffer.spec();
    let rate = spec.rate();
    let channels = spec.channels().count();
    interleaved.clear();
    mono.clear();
    if channels == 0 {
        return (rate, 0);
    }
    buffer.copy_to_vec_interleaved(interleaved);
    #[allow(clippy::cast_precision_loss)]
    let scale = 1.0 / channels as f32;
    mono.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * scale),
    );
    (rate, channels)
}

fn open(source: &Source) -> Result<(MediaSourceStream<'static>, Hint), String> {
    let mut hint = Hint::new();
    match source {
        Source::File(path) => {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                hint.with_extension(ext);
            }
            let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((
                MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default()),
                hint,
            ))
        }
        Source::Url(url) => {
            let response = ureq::get(url)
                .call()
                .map_err(|e| format!("fetching it: {e}"))?;
            if let Some(mime) = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
            {
                let mime = mime.split(';').next().unwrap_or(mime).trim();
                // A YouTube, Spotify, SoundCloud or Bandcamp link is the
                // page about the track, not the track: say so, instead of
                // symphonia's "no suitable format reader" after a megabyte
                // of HTML.
                if mime.eq_ignore_ascii_case("text/html") {
                    return Err(
                        "that is a web page, not an audio file — I play direct links \
                         to mp3/ogg/flac/m4a/wav files and radio streams (YouTube and \
                         the like need music_bot.py, which uses yt-dlp)"
                            .to_string(),
                    );
                }
                hint.mime_type(mime);
            }
            if let Some(ext) = url
                .rsplit('/')
                .next()
                .and_then(|name| name.split('?').next())
                .and_then(|name| name.rsplit_once('.'))
                .map(|(_, ext)| ext)
                .filter(|ext| ext.len() <= 4)
            {
                hint.with_extension(ext);
            }
            let reader = response.into_body().into_reader();
            Ok((
                MediaSourceStream::new(
                    Box::new(ReadOnlySource::new(reader)),
                    MediaSourceStreamOptions::default(),
                ),
                hint,
            ))
        }
    }
}

/// Mono at any rate → mono at 48 kHz, the way the audio engine's
/// input resampler is built (sinc, fixed-size input chunks).
struct Mono48k {
    rate: u32,
    inner: Async<f32>,
    pending: Vec<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    out: Vec<f32>,
}

/// Input frames per resampler call: ~21 ms at 48 kHz, small enough
/// that a track starts within a few frames.
const RESAMPLE_CHUNK: usize = 1024;

impl Mono48k {
    fn new(rate: u32) -> Result<Self, String> {
        let params = SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: Some(0.95),
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let inner = Async::<f32>::new_sinc(
            48_000.0 / f64::from(rate),
            1.0,
            &params,
            RESAMPLE_CHUNK,
            1,
            FixedAsync::Input,
        )
        .map_err(|e| format!("no resampler from {rate} Hz: {e}"))?;
        let output = vec![vec![0.0f32; inner.output_frames_max()]];
        Ok(Self {
            rate,
            inner,
            pending: Vec::with_capacity(RESAMPLE_CHUNK * 2),
            input: vec![Vec::with_capacity(RESAMPLE_CHUNK)],
            output,
            out: Vec::new(),
        })
    }

    /// Feed samples; what came out so far at 48 kHz.
    fn push(&mut self, samples: &[f32]) -> &[f32] {
        use audioadapter_buffers::direct::SequentialSliceOfVecs;
        self.out.clear();
        self.pending.extend_from_slice(samples);
        loop {
            let needed = self.inner.input_frames_next();
            if self.pending.len() < needed {
                break;
            }
            self.input[0].clear();
            self.input[0].extend_from_slice(&self.pending[..needed]);
            self.pending.drain(..needed);
            let expected = self.inner.output_frames_next();
            self.output[0].resize(expected, 0.0);
            let Ok(input) = SequentialSliceOfVecs::new(&self.input, 1, needed) else {
                break;
            };
            let Ok(mut output) = SequentialSliceOfVecs::new_mut(&mut self.output, 1, expected)
            else {
                break;
            };
            match self.inner.process_into_buffer(&input, &mut output, None) {
                Ok((_, produced)) => self.out.extend_from_slice(&self.output[0][..produced]),
                Err(e) => {
                    warn!("resampling: {e}");
                    break;
                }
            }
        }
        &self.out
    }
}

// ── The chat side ───────────────────────────────────────────────────────

/// Who is where, from `READY` and the roster events after it.
#[derive(Default)]
struct Room {
    me: String,
    /// `client_id` → channel (`None` in the lobby).
    channels: HashMap<String, Option<u16>>,
    names: HashMap<String, String>,
}

impl Room {
    fn learn(&mut self, t: &str, d: &Value) {
        let id = |d: &Value| d["client_id"].as_str().unwrap_or("").to_string();
        let channel = |d: &Value| d["channel_id"].as_u64().and_then(|c| u16::try_from(c).ok());
        match t {
            "READY" => {
                self.me = id(d);
                self.channels.clear();
                for m in d["members"].as_array().into_iter().flatten() {
                    self.channels.insert(id(m), channel(m));
                    self.names
                        .insert(id(m), m["name"].as_str().unwrap_or("?").to_string());
                }
            }
            "MEMBER_ADD" => {
                self.channels.insert(id(d), channel(d));
                self.names
                    .insert(id(d), d["name"].as_str().unwrap_or("?").to_string());
            }
            "VOICE_STATE" => {
                self.channels.insert(id(d), channel(d));
            }
            "MEMBER_UPDATE" => {
                if let Some(name) = d["name"].as_str() {
                    self.names.insert(id(d), name.to_string());
                }
            }
            "MEMBER_REMOVE" => {
                self.channels.remove(&id(d));
                self.names.remove(&id(d));
            }
            _ => {}
        }
    }

    fn channel_of(&self, client_id: &str) -> Option<u16> {
        self.channels.get(client_id).copied().flatten()
    }
}

const HELP: &str = "!play <file or url> · !skip · !stop · !queue";

fn resolve(arg: &str, library: Option<&Path>) -> Result<Source, String> {
    if arg.starts_with("http://") || arg.starts_with("https://") {
        return Ok(Source::Url(arg.to_string()));
    }
    let Some(library) = library else {
        return Err("files are off (no --library); URLs only".to_string());
    };
    let root = library
        .canonicalize()
        .map_err(|e| format!("the library folder: {e}"))?;
    let path = root
        .join(arg)
        .canonicalize()
        .map_err(|_| format!("no such file in the library: {arg}"))?;
    if !path.starts_with(&root) {
        return Err("that is outside the library".to_string());
    }
    Ok(Source::File(path))
}

fn label_of(source: &Source) -> String {
    match source {
        Source::File(path) => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        Source::Url(url) => url.clone(),
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            error!("{e}");
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: Args) -> Result<ExitCode, String> {
    let name = protocol::sanitize_username(&args.name);
    if name.is_empty() {
        return Err("--name is empty".to_string());
    }
    let server = args
        .server
        .to_socket_addrs()
        .map_err(|e| format!("--server {:?}: {e}", args.server))?
        .next()
        .ok_or_else(|| format!("--server {:?} resolves to nothing", args.server))?;
    let key = Arc::new(ClientKey::load_or_create(&args.key).map_err(|e| format!("the key: {e}"))?);
    let pin_file = args.key.with_extension("pin");
    let pin = match args.pin.as_deref() {
        Some(text) => Some(
            text.parse::<Fingerprint>()
                .map_err(|()| format!("--pin {text:?} is not SHA256:<base64>"))?,
        ),
        None => std::fs::read_to_string(&pin_file)
            .ok()
            .and_then(|text| text.trim().parse::<Fingerprint>().ok()),
    };
    let config = Config {
        server,
        pin,
        pin_file: Some(pin_file),
        name,
        invite: args.invite,
        key,
    };
    let bot = Bot::connect(config, args.voice_kbps).map_err(|e| format!("starting: {e}"))?;
    let (quit_tx, quit) = crossbeam_channel::bounded::<()>(1);
    ctrlc::set_handler(move || {
        let _ = quit_tx.try_send(());
    })
    .map_err(|e| format!("could not take Ctrl-C: {e}"))?;
    let player = Player::start(&bot);
    let actor = bot.actor();
    let library = args.library.as_deref();
    let mut room = Room::default();
    let mut my_channel: Option<u16> = None;

    loop {
        let event = crossbeam_channel::select! {
            recv(bot.events()) -> event => match event {
                Ok(event) => event,
                // The session ended for good: the status said why.
                Err(_) => break,
            },
            recv(quit) -> _ => {
                info!("shutting down");
                player.stop();
                let _ = bot.shutdown();
                return Ok(ExitCode::SUCCESS);
            }
        };
        match event {
            Outbound::Status { t, d } => {
                info!("{t}: {d}");
                if t == "REFUSED" || t == "REMOVED" {
                    break;
                }
            }
            Outbound::Error { d } => warn!("{d}"),
            Outbound::Dispatch { t, d } => {
                if t == "RATE_LIMITED" {
                    warn!("the server dropped a line of ours (chat limit): {d}");
                }
                room.learn(t, &d);
                if t == "VOICE_STATE" && d["client_id"].as_str() == Some(room.me.as_str()) {
                    my_channel = d["channel_id"].as_u64().and_then(|c| u16::try_from(c).ok());
                }
                if t != "MESSAGE_CREATE" || d["own"].as_bool() == Some(true) {
                    continue;
                }
                let text = d["text"].as_str().unwrap_or("").trim();
                let Some(command) = text.strip_prefix('!') else {
                    continue;
                };
                let (verb, arg) = command.split_once(' ').unwrap_or((command, ""));
                let asker = d["name"].as_str().unwrap_or("someone").to_string();
                let asker_id = d["client_id"].as_str().unwrap_or("");
                let say = |text: String| actor.act(Action::CreateMessage { text });
                match (verb, arg.trim()) {
                    ("play", "") | ("help", _) => say(HELP.to_string()),
                    ("play", arg) => {
                        let Some(channel) = room.channel_of(asker_id) else {
                            say(format!("{asker}, join a voice channel first."));
                            continue;
                        };
                        match resolve(arg, library) {
                            Ok(source) => {
                                if my_channel != Some(channel) {
                                    actor.act(Action::JoinChannel {
                                        channel_id: channel,
                                        password: None,
                                    });
                                }
                                let label = label_of(&source);
                                let place = player.play(Request {
                                    source,
                                    label: label.clone(),
                                    asker,
                                });
                                if let Some(place) = place {
                                    say(format!("Queued ({place}): {label}"));
                                }
                            }
                            Err(e) => say(e),
                        }
                    }
                    ("skip", _) => player.skip(),
                    ("stop", _) => player.stop(),
                    ("queue", _) => say(format!("{} queued.", player.queued())),
                    _ => {}
                }
            }
            Outbound::Hello { .. } => {}
        }
    }
    Ok(ExitCode::from(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 s stereo 440 Hz tone at 44.1 kHz comes out as ~1 s of mono
    /// 48 kHz at the same level.
    #[test]
    fn a_wav_decodes_to_mono_48k() {
        let dir = std::env::temp_dir().join(format!("baucord-music-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for n in 0..44_100u32 {
            #[allow(clippy::cast_precision_loss)]
            let s = (0.5
                * (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 44_100.0).sin()
                * 32_767.0) as i16;
            writer.write_sample(s).unwrap();
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();

        let mut out: Vec<f32> = Vec::new();
        decode(&Source::File(path.clone()), &mut || false, &mut |samples| {
            out.extend_from_slice(samples);
            true
        })
        .unwrap();
        // The resampler keeps a tail (sinc delay, a last partial chunk).
        assert!(
            (45_000..=48_500).contains(&out.len()),
            "{} samples for 1 s",
            out.len()
        );
        #[allow(clippy::cast_precision_loss)]
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!((0.30..=0.40).contains(&rms), "rms {rms}, expected ~0.354");

        // `!skip` ends a decode early.
        let mut taken = 0usize;
        let skip = AtomicBool::new(false);
        decode(
            &Source::File(path),
            &mut || skip.load(Ordering::Relaxed),
            &mut |samples| {
                taken += samples.len();
                skip.store(true, Ordering::Relaxed);
                true
            },
        )
        .unwrap();
        assert!(taken < 20_000, "{taken} samples after a skip");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn files_stay_inside_the_library() {
        let dir = std::env::temp_dir().join(format!("baucord-lib-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.mp3"), b"").unwrap();
        assert!(matches!(
            resolve("sub/a.mp3", Some(&dir)).unwrap(),
            Source::File(_)
        ));
        assert!(resolve("../../etc/passwd", Some(&dir)).is_err());
        assert!(resolve("/etc/passwd", Some(&dir)).is_err());
        assert!(resolve("a.mp3", None).unwrap_err().contains("URLs only"));
        assert!(matches!(
            resolve("https://x.example/a.mp3", None).unwrap(),
            Source::Url(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
