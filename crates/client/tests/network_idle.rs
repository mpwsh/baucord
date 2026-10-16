//! The network thread sleeps on a quiet connection and still hears a
//! datagram at once (IOS-SPEC.md §14 D41), on every platform: Windows
//! has its own socket wait since review P1-1.
//!
//! A bare QUIC endpoint on loopback stands in for the server: enough
//! for the handshake, the client's `Hello`, a `Welcome` that admits it,
//! and voice datagrams back. The client is the real network thread,
//! driven through its channels.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{VoiceChat, VoiceChatConfig};
use client::{
    network::{self, NetworkControl, NetworkThreadHandle, VoiceLink},
    voice::VoiceControl,
};
use protocol::Lane;
use transport::{Budget, ClientKey, Driver, Event, PeerId, ServerIdentity, UdpIo};

/// What the stand-in server saw, and what it is asked to send.
#[derive(Default)]
struct Shared {
    client: Option<PeerId>,
    hello_bytes: usize,
    /// The `Welcome` went out: the client is admitted.
    welcomed: bool,
    /// Voice datagrams to send now, with the moment they were queued.
    outbox: Vec<(Vec<u8>, Instant)>,
    sent_at: Vec<Instant>,
}

fn serve(stop: &AtomicBool, shared: &Mutex<Shared>, udp: &mut UdpIo, driver: &mut Driver) {
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let _ = udp.recv_batch(|d| driver.handle_datagram(now, d));
        driver.handle_timeouts(now);
        let mut s = shared.lock().unwrap();
        for ev in driver.drain_events() {
            if let Event::Connected { peer, .. } = ev {
                s.client = Some(peer);
            }
        }
        if let Some(peer) = s.client {
            while let Some(frame) = driver.recv(peer, Lane::Control) {
                s.hello_bytes += frame.len();
            }
            // Admit it, as a server does: what the client runs once
            // admitted (link stats, telemetry, reports) is what an idle
            // session costs. A connection still at its Hello did not
            // show it (review 2026-10, P3-46).
            if s.hello_bytes > 0 && !s.welcomed {
                s.welcomed = true;
                let welcome = protocol::ServerMessage::Welcome {
                    client_id: 42,
                    user_id: None,
                    features: protocol::features::SERVER_1_1,
                };
                let bytes = protocol::encode_server_message(&welcome).expect("welcome");
                let _ = driver.send(now, peer, Lane::Control, &bytes);
            }
            for (bytes, _) in std::mem::take(&mut s.outbox) {
                let _ = driver.send(now, peer, Lane::Voice, &bytes);
                s.sent_at.push(Instant::now());
            }
        }
        drop(s);
        while let Some(t) = driver.poll_transmit() {
            udp.send(&t);
        }
        thread::sleep(Duration::from_micros(500));
    }
}

#[test]
fn a_quiet_connection_sleeps_and_still_hears_a_datagram_at_once() {
    // ── The stand-in server ────────────────────────────────────────
    let identity = ServerIdentity::generate();
    let mut server = Driver::new_server(&identity, 4, Budget::server()).expect("server");
    let mut udp = UdpIo::bind("127.0.0.1:0".parse().unwrap(), 1 << 20).expect("bind");
    let addr: SocketAddr = udp.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Mutex::new(Shared::default()));
    let server_thread = {
        let (stop, shared) = (Arc::clone(&stop), Arc::clone(&shared));
        thread::spawn(move || serve(&stop, &shared, &mut udp, &mut server))
    };

    // ── The client's network thread ────────────────────────────────
    let (tx, control_rx) = crossbeam_channel::unbounded();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
    let (voice_tx, _voice_rx) = crossbeam_channel::unbounded::<VoiceControl>();
    let (screen_tx, _screen_rx) = crossbeam_channel::unbounded();
    let (_media_tx, media_rx) = crossbeam_channel::bounded(client::network::MEDIA_QUEUE);
    let mut net = NetworkThreadHandle::spawn(
        tx,
        client::network::NetworkInputs {
            control: control_rx,
            media: media_rx,
        },
        msg_tx,
        msg_rx,
        voice_tx,
        screen_tx,
        Arc::new(|| {}),
    );
    // A headless engine, linked the way the voice thread links one
    // (review P1-5): received voice goes straight into it.
    let engine = VoiceChat::new(VoiceChatConfig::default());
    let received = Arc::clone(engine.stats());
    net.tx
        .send(NetworkControl::VoiceLink(Some(VoiceLink {
            incoming: engine.incoming_sender(),
            outgoing: engine.outgoing_receiver(),
            testing: Arc::new(AtomicBool::new(false)),
            local_only: false,
        })))
        .expect("link");
    net.tx
        .send(NetworkControl::Connect {
            addr,
            nickname: "sleeper".into(),
            invite_code: None,
            fingerprint: None,
            client_key: Arc::new(ClientKey::generate()),
            portable: false,
        })
        .expect("connect");

    // The handshake, the Hello and the Welcome: inbound datagrams reach
    // the client.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shared.lock().unwrap().welcomed {
        assert!(Instant::now() < deadline, "the client never said Hello");
        thread::sleep(Duration::from_millis(10));
    }
    // Let the handshake's and the admission's tail (acks, lane tags,
    // the first telemetry) settle.
    thread::sleep(Duration::from_secs(2));

    // ── Asleep: a quiet connection wakes only when something is due —
    // the 5 s keep-alive and its ack, the 2 s link stats (review P1-2);
    // the inbound watcher no longer looks twice a second (review
    // 2026-10, P2-40). The 100 ms
    // quiet tick made it about twelve; polling the socket, as Windows
    // did until review P1-1, a thousand.
    // Measured over the idle steady state, long enough to hold the 5 s
    // keep-alive and the 10 s telemetry: one second, unadmitted, saw
    // neither (review 2026-10, P3-46).
    let window = Duration::from_secs(12);
    let (passes_before, watcher_before) = (network::loop_passes(), network::watcher_wakes());
    thread::sleep(window);
    let passes = network::loop_passes() - passes_before;
    let watcher = network::watcher_wakes() - watcher_before;
    #[allow(clippy::cast_precision_loss)]
    let per_second = (passes + watcher) as f64 / window.as_secs_f64();
    eprintln!(
        "network loop: {passes} wakes and {watcher} watcher wakes in {window:?} ({per_second:.1}/s)"
    );
    assert!(
        per_second < 5.0,
        "{passes} loop and {watcher} watcher wakes in {window:?} (something polls)"
    );

    // ── Awake on arrival: a voice datagram reaches the engine at once,
    // not at the next deadline.
    let mut delays = Vec::new();
    for seq in 0..8u32 {
        let before = received.packets_received.load(Ordering::Relaxed);
        let packet = protocol::pack_voice_with_flags(seq, seq * 960, 0, &[0xF8, 0xFF, 0xFE])
            .expect("voice packet");
        let wrapped = protocol::wrap_sender(42, &packet);
        shared
            .lock()
            .unwrap()
            .outbox
            .push((wrapped, Instant::now()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while received.packets_received.load(Ordering::Relaxed) == before {
            assert!(Instant::now() < deadline, "no voice packet");
            thread::yield_now();
        }
        let sent = *shared.lock().unwrap().sent_at.last().expect("sent");
        delays.push(sent.elapsed());
        thread::sleep(Duration::from_millis(150));
    }
    delays.sort();
    let median = delays[delays.len() / 2];
    eprintln!("datagram → engine: median {median:?}, all {delays:?}");
    assert!(
        median < Duration::from_millis(30),
        "a datagram took {median:?} (median) to wake the loop; delays {delays:?}"
    );

    net.shutdown();
    stop.store(true, Ordering::Relaxed);
    server_thread.join().expect("server thread");
}
