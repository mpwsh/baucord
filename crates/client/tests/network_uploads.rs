//! Every upload keeps its own state (review P1-17): two files sent
//! together both arrive, `CancelImage` still stops a picture after a
//! file upload, and cancelling a file stops that file, not the picture
//! going out beside it.
//!
//! A bare QUIC endpoint on loopback stands in for the server: it
//! accepts every announced upload and counts the chunks that reach it.
//! The client is the real network thread, driven through its channels.

use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use client::{
    network::{NetworkControl, NetworkThreadHandle},
    voice::VoiceControl,
};
use protocol::{ClientMessage, Lane, ServerMessage};
use transport::{Budget, ClientKey, Driver, Event, PeerId, ServerIdentity, UdpIo};

/// What reached the stand-in server.
#[derive(Default)]
struct Seen {
    client: Option<PeerId>,
    hello: bool,
    /// Blob uploads: upload id → (chunk count, chunk indexes received).
    blobs: HashMap<u32, (u32, BTreeSet<u32>)>,
    /// Picture chunks received since the last reset, and their count.
    image_chunks: u32,
    image_count: u32,
}

impl Seen {
    fn blob_done(&self, upload_id: u32) -> bool {
        self.blobs
            .get(&upload_id)
            .is_some_and(|(count, got)| got.len() == *count as usize)
    }

    fn blob_chunks(&self, upload_id: u32) -> usize {
        self.blobs.get(&upload_id).map_or(0, |(_, got)| got.len())
    }
}

fn serve(stop: &AtomicBool, seen: &Mutex<Seen>, udp: &mut UdpIo, driver: &mut Driver) {
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let _ = udp.recv_batch(|d| driver.handle_datagram(now, d));
        driver.handle_timeouts(now);
        let mut s = seen.lock().unwrap();
        for ev in driver.drain_events() {
            if let Event::Connected { peer, .. } = ev {
                s.client = Some(peer);
            }
        }
        if let Some(peer) = s.client {
            for lane in [Lane::Control, Lane::Signal, Lane::Media] {
                while let Some(frame) = driver.recv(peer, lane) {
                    match protocol::decode_client_message(&frame) {
                        Some(ClientMessage::Hello { .. }) => s.hello = true,
                        Some(ClientMessage::UploadBegin { upload_id, .. }) => {
                            let accept = ServerMessage::UploadAccept {
                                upload_id,
                                needed: true,
                            };
                            let bytes = protocol::encode_server_message(&accept).expect("encode");
                            driver
                                .send(now, peer, accept.lane(), &bytes)
                                .expect("accept");
                        }
                        Some(ClientMessage::UploadChunk {
                            upload_id,
                            index,
                            count,
                            ..
                        }) => {
                            let entry = s
                                .blobs
                                .entry(upload_id)
                                .or_insert_with(|| (count, BTreeSet::new()));
                            entry.1.insert(index);
                        }
                        Some(ClientMessage::ChatImageChunk { count, .. }) => {
                            s.image_chunks += 1;
                            s.image_count = count;
                        }
                        _ => {}
                    }
                }
            }
        }
        drop(s);
        while let Some(t) = driver.poll_transmit() {
            udp.send(&t);
        }
        thread::sleep(Duration::from_micros(500));
    }
}

/// Wait up to `timeout` for `done`.
fn wait_for(seen: &Mutex<Seen>, timeout: Duration, done: impl Fn(&Seen) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done(&seen.lock().unwrap()) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done(&seen.lock().unwrap())
}

fn upload(upload_id: u32, data: Vec<u8>) -> NetworkControl {
    NetworkControl::UploadBlob {
        upload_id,
        name: format!("file-{upload_id}.bin"),
        mime: "application/octet-stream".into(),
        data,
    }
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

#[test]
fn uploads_keep_their_own_state() {
    // ── The stand-in server ────────────────────────────────────────
    let identity = ServerIdentity::generate();
    let mut server = Driver::new_server(&identity, 4, Budget::server()).expect("server");
    let mut udp = UdpIo::bind("127.0.0.1:0".parse().unwrap(), 1 << 20).expect("bind");
    let addr: SocketAddr = udp.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let server_thread = {
        let (stop, seen) = (Arc::clone(&stop), Arc::clone(&seen));
        thread::spawn(move || serve(&stop, &seen, &mut udp, &mut server))
    };

    // ── The client's network thread ────────────────────────────────
    let (tx, control_rx) = crossbeam_channel::unbounded();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
    let (voice_tx, _voice_rx) = crossbeam_channel::unbounded::<VoiceControl>();
    let (screen_tx, _screen_rx) = crossbeam_channel::unbounded();
    let mut net = NetworkThreadHandle::spawn(
        tx,
        control_rx,
        msg_tx,
        msg_rx,
        voice_tx,
        screen_tx,
        Arc::new(|| {}),
    );
    net.tx
        .send(NetworkControl::Connect {
            addr,
            nickname: "uploader".into(),
            invite_code: None,
            fingerprint: None,
            client_key: Arc::new(ClientKey::generate()),
            portable: false,
        })
        .expect("connect");
    assert!(
        wait_for(&seen, Duration::from_secs(10), |s| s.hello),
        "the client never said Hello"
    );

    // ── Two files at once: both arrive whole. The second used to take
    // the first's slot, and the first sat at 0 % for good.
    net.tx.send(upload(101, bytes(40 * 1024, 1))).expect("send");
    net.tx.send(upload(102, bytes(40 * 1024, 2))).expect("send");
    let both = wait_for(&seen, Duration::from_secs(10), |s| {
        s.blob_done(101) && s.blob_done(102)
    });
    let got = {
        let s = seen.lock().unwrap();
        (s.blob_chunks(101), s.blob_chunks(102))
    };
    assert!(both, "not both files arrived: {got:?} chunks of 3 each");

    // ── A picture after a file upload can still be cancelled. The
    // file's leftover slot used to make `CancelImage` do nothing, and
    // the picture went out in full: 1 MiB is two seconds of pacing.
    net.tx
        .send(NetworkControl::SendImage(bytes(1024 * 1024, 3)))
        .expect("send");
    net.tx.send(NetworkControl::CancelImage).expect("send");
    thread::sleep(Duration::from_secs(3));
    let (image_chunks, image_count) = {
        let s = seen.lock().unwrap();
        (s.image_chunks, s.image_count)
    };
    assert!(
        image_chunks < 16,
        "the cancelled picture kept going: {image_chunks} of {image_count} chunks"
    );

    // ── Cancelling a file stops that file, and the picture queued
    // behind it goes. The cancel used to reset the picture's stream.
    net.tx
        .send(upload(103, bytes(1024 * 1024, 4)))
        .expect("send");
    assert!(
        wait_for(&seen, Duration::from_secs(5), |s| s.blob_chunks(103) > 0),
        "the big file never started"
    );
    {
        let mut s = seen.lock().unwrap();
        s.image_chunks = 0;
        s.image_count = 0;
    }
    net.tx
        .send(NetworkControl::SendImage(bytes(20 * 1024, 5)))
        .expect("send");
    net.tx
        .send(NetworkControl::CancelUpload { upload_id: 103 })
        .expect("send");
    let arrived = wait_for(&seen, Duration::from_secs(5), |s| {
        s.image_count > 0 && s.image_chunks == s.image_count
    });
    let (image_chunks, file_chunks) = {
        let s = seen.lock().unwrap();
        (s.image_chunks, s.blob_chunks(103))
    };
    assert!(
        arrived,
        "the picture beside the cancelled file never arrived: {image_chunks} chunks"
    );
    assert!(
        file_chunks < 32,
        "the cancelled file kept going: {file_chunks} of 64 chunks"
    );

    net.shutdown();
    stop.store(true, Ordering::Relaxed);
    server_thread.join().expect("server thread");
}
