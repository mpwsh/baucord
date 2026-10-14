//! Acceptance scenarios (spec §10) that need no NIC.

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::Bytes;
use protocol::Lane;
use rcgen::PublicKeyData;

use super::{LinkSpec, NodeId, SimNet};
use crate::{
    config::Budget,
    driver::{
        DisconnectReason, Driver, Event, PeerId, Shared, TrackStats, VideoItem, WaitingRoom,
        OWN_TRACK, VIDEO_GROUP_DEADLINE,
    },
    identity::{Fingerprint, Observation, ServerIdentity, Trust},
};

const TICK: Duration = Duration::from_millis(2);
const SERVER_ADDR: &str = "10.0.0.1:4433";
const CLIENT_ADDR: &str = "10.0.0.2:5000";

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("addr")
}

fn init_log() {
    let _ = env_logger::builder().is_test(true).try_init();
}

struct Pair {
    net: SimNet,
    server: NodeId,
    client: NodeId,
    identity: ServerIdentity,
}

fn pair_with(
    seed: u64,
    link: LinkSpec,
    client_driver: impl FnOnce(&ServerIdentity) -> Driver,
) -> Pair {
    init_log();
    let identity = ServerIdentity::generate();
    let server_driver = Driver::new_server(&identity, 8, Budget::server()).expect("server driver");
    let client_driver = client_driver(&identity);
    let mut net = SimNet::new(seed);
    let server = net.add_node(server_driver, addr(SERVER_ADDR));
    let client = net.add_node(client_driver, addr(CLIENT_ADDR));
    net.set_link(server, client, link);
    Pair {
        net,
        server,
        client,
        identity,
    }
}

fn pair(seed: u64, link: LinkSpec) -> Pair {
    pair_with(seed, link, |id| {
        Driver::new_client(Some(id.fingerprint()), Budget::client()).expect("client driver")
    })
}

/// Connect and wait for both sides. Returns `(client's peer id for the
/// server, server's peer id for the client)`.
fn connect(p: &mut Pair) -> (PeerId, PeerId) {
    let now = p.net.now();
    let server_peer = p
        .net
        .driver(p.client)
        .connect(now, addr(SERVER_ADDR))
        .expect("connect");
    let (server, client) = (p.server, p.client);
    let mut client_peer = None;
    let ok = p.net.run_until(Duration::from_secs(5), TICK, |net| {
        for ev in net.driver(server).drain_events() {
            if let Event::Connected { peer, .. } = ev {
                client_peer = Some(peer);
            }
        }
        client_peer.is_some() && net.driver(client).is_connected(server_peer)
    });
    assert!(ok, "handshake did not complete");
    // Let the client's lane-tag bytes land on the server.
    p.net.run_for(Duration::from_millis(200), TICK);
    (server_peer, client_peer.expect("accepted"))
}

fn drain_client_events(p: &mut Pair) -> Vec<Event> {
    let client = p.client;
    p.net.driver(client).drain_events().collect()
}

#[test]
fn handshake_then_control_and_signal_frames_roundtrip() {
    let mut p = pair(1, LinkSpec::clean(Duration::from_millis(20)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let now = p.net.now();
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Control, b"hello")
        .expect("send control");
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Signal, b"sub")
        .expect("send signal");

    let mut got = Vec::new();
    let ok = p.net.run_until(Duration::from_secs(2), TICK, |net| {
        while let Some(f) = net.driver(server).recv(client_peer, Lane::Control) {
            got.push((Lane::Control, f));
        }
        while let Some(f) = net.driver(server).recv(client_peer, Lane::Signal) {
            got.push((Lane::Signal, f));
        }
        got.len() == 2
    });
    assert!(ok, "server did not get both frames: {got:?}");
    assert!(got.contains(&(Lane::Control, Bytes::from_static(b"hello"))));
    assert!(got.contains(&(Lane::Signal, Bytes::from_static(b"sub"))));

    // And back.
    let now = p.net.now();
    p.net
        .driver(server)
        .send(now, client_peer, Lane::Control, b"welcome")
        .expect("server send");
    let mut back = None;
    let ok = p.net.run_until(Duration::from_secs(2), TICK, |net| {
        back = net.driver(client).recv(server_peer, Lane::Control);
        back.is_some()
    });
    assert!(ok);
    assert_eq!(back.as_deref(), Some(&b"welcome"[..]));
}

#[test]
fn voice_datagrams_flow_both_ways() {
    let mut p = pair(2, LinkSpec::clean(Duration::from_millis(30)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let now = p.net.now();
    for i in 0..10u8 {
        p.net
            .driver(client)
            .send(now, server_peer, Lane::Voice, &[i; 100])
            .expect("voice");
        p.net
            .driver(server)
            .send(now, client_peer, Lane::SystemAudio, &[i; 300])
            .expect("sysaudio");
    }
    let mut at_server = Vec::new();
    let mut at_client = Vec::new();
    p.net.run_until(Duration::from_secs(1), TICK, |net| {
        while let Some(d) = net.driver(server).recv(client_peer, Lane::Voice) {
            at_server.push(d);
        }
        while let Some(d) = net.driver(client).recv(server_peer, Lane::SystemAudio) {
            at_client.push(d);
        }
        at_server.len() == 10 && at_client.len() == 10
    });
    assert_eq!(at_server.len(), 10);
    assert_eq!(at_client.len(), 10);
    assert_eq!(at_server[3].len(), 100);
    assert_eq!(at_client[3].len(), 300);
    // Nothing leaked across lanes.
    assert!(p
        .net
        .driver(server)
        .recv(client_peer, Lane::SystemAudio)
        .is_none());
    assert!(p
        .net
        .driver(client)
        .recv(server_peer, Lane::Voice)
        .is_none());
}

#[test]
fn oversize_datagram_is_refused_at_send_time() {
    let mut p = pair(3, LinkSpec::clean(Duration::from_millis(5)));
    let (server_peer, _) = connect(&mut p);
    let client = p.client;
    let now = p.net.now();
    let err = p
        .net
        .driver(client)
        .send(now, server_peer, Lane::Voice, &[0; 2000])
        .unwrap_err();
    assert!(matches!(
        err,
        crate::driver::SendError::TooLarge(Lane::Voice, 2000)
    ));
}

#[test]
fn video_group_delivers_start_chunks_end_in_order() {
    let mut p = pair(4, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let now = p.net.now();
    p.net.driver(client).video_new_group(now, server_peer, 0);
    for i in 0..5u8 {
        p.net
            .driver(client)
            .send(now, server_peer, Lane::Video, &[i; 1000])
            .expect("chunk");
    }
    p.net.driver(client).video_finish_group(now, server_peer, 0);

    let mut items = Vec::new();
    p.net.run_until(Duration::from_secs(2), TICK, |net| {
        while let Some((track, it)) = net.driver(server).recv_video(client_peer) {
            assert_eq!(track, 0);
            items.push(it);
        }
        matches!(items.last(), Some(VideoItem::GroupEnd))
    });
    assert_eq!(items.len(), 7, "{items:?}");
    assert_eq!(items[0], VideoItem::GroupStart);
    for (i, it) in items[1..6].iter().enumerate() {
        assert_eq!(
            *it,
            VideoItem::Chunk(Bytes::from(vec![u8::try_from(i).unwrap(); 1000]))
        );
    }
    assert_eq!(items[6], VideoItem::GroupEnd);
}

#[test]
fn starting_a_new_group_while_the_old_is_unsent_resets_it() {
    // Uplink 2 Mbit/s; a 200 KB group takes ~800 ms to drain — the next
    // keyframe arrives after 100 ms, so the old group must be reset.
    let link = LinkSpec::clean(Duration::from_millis(10));
    let mut p = pair(5, link);
    p.net.set_link_one_way(
        p.client,
        p.server,
        LinkSpec::clean(Duration::from_millis(10)).with_bandwidth(2_000_000, 200),
    );
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    for _ in 0..3 {
        let now = p.net.now();
        p.net.driver(client).video_new_group(now, server_peer, 0);
        for _ in 0..200 {
            p.net
                .driver(client)
                .send(now, server_peer, Lane::Video, &[7; 1000])
                .expect("chunk");
        }
        p.net.run_for(Duration::from_millis(100), TICK);
    }
    // A small fresh group after the storm must get through.
    let now = p.net.now();
    p.net.driver(client).video_new_group(now, server_peer, 0);
    for _ in 0..5 {
        p.net
            .driver(client)
            .send(now, server_peer, Lane::Video, &[9; 1000])
            .expect("chunk");
    }
    p.net.driver(client).video_finish_group(now, server_peer, 0);
    p.net.run_for(Duration::from_secs(3), TICK);

    let stats = p.net.driver(client).stats(server_peer).expect("stats");
    assert_eq!(stats.groups_started, 4);
    assert!(stats.groups_reset >= 2, "{stats:?}");
    assert_eq!(
        stats.groups_reset,
        stats.groups_reset_superseded + stats.groups_reset_deadline + stats.groups_reset_unacked,
        "every behind-reset has exactly one cause: {stats:?}"
    );
    assert_eq!(stats.groups_reset_upstream, 0);
    let mut items = Vec::new();
    while let Some((_, it)) = p.net.driver(server).recv_video(client_peer) {
        items.push(it);
    }
    let resets = items
        .iter()
        .filter(|i| **i == VideoItem::GroupReset)
        .count();
    assert!(resets >= 2, "{items:?}");
    // The fresh group completed, in order, with its five chunks.
    let last_start = items
        .iter()
        .rposition(|i| *i == VideoItem::GroupStart)
        .expect("a group start");
    let tail = &items[last_start..];
    assert_eq!(tail.len(), 7, "{tail:?}");
    assert!(tail[1..6]
        .iter()
        .all(|i| *i == VideoItem::Chunk(Bytes::from(vec![9; 1000]))));
    assert_eq!(tail[6], VideoItem::GroupEnd);
}

/// What the application queues in one pass leaves in one pass: five
/// voice packets and a control frame for one peer are one drain of its
/// connection and one UDP datagram, where each send used to be a drain
/// and a datagram of its own (review P2-31). And a payload shared
/// between sends arrives as the one built per send does (P2-32).
#[test]
fn one_pass_of_sends_is_one_packet_and_shared_bytes_arrive_whole() {
    let mut p = pair(18, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    // Let the handshake's tail (acks, the lanes' streams) settle.
    p.net.run_until(Duration::from_millis(300), TICK, |_| false);

    let before = p.net.driver(server).stats(client_peer).expect("stats");
    let now = p.net.now();
    let voice = Shared::relayed(Lane::Voice, 7, &[9; 60]);
    assert_eq!(voice.len(), 68, "the sender's id and the packet");
    for _ in 0..5 {
        p.net
            .driver(server)
            .send_shared(now, client_peer, &voice)
            .expect("voice");
    }
    let hello = Shared::new(Lane::Control, b"hello");
    p.net
        .driver(server)
        .send_shared(now, client_peer, &hello)
        .expect("control");
    // One pass of the loop.
    p.net.step(TICK);
    let after = p.net.driver(server).stats(client_peer).expect("stats");
    assert_eq!(
        after.sent_packets - before.sent_packets,
        1,
        "six sends, one packet"
    );

    let mut voices = Vec::new();
    let mut control = None;
    p.net.run_until(Duration::from_secs(1), TICK, |net| {
        while let Some(packet) = net.driver(client).recv(server_peer, Lane::Voice) {
            voices.push(packet);
        }
        if let Some(frame) = net.driver(client).recv(server_peer, Lane::Control) {
            control = Some(frame);
        }
        voices.len() == 5 && control.is_some()
    });
    assert_eq!(voices.len(), 5);
    for packet in &voices {
        assert_eq!(packet[..], protocol::wrap_sender(7, &[9; 60])[..]);
    }
    assert_eq!(control.as_deref(), Some(&b"hello"[..]));

    // A payload made for one lane does not go out as another's.
    let now = p.net.now();
    assert!(p
        .net
        .driver(server)
        .send_video_shared(now, client_peer, 3, &voice)
        .is_err());
}

#[test]
fn group_counters_are_kept_per_track() {
    let mut p = pair(16, LinkSpec::clean(Duration::from_millis(10)));
    let (_, client_peer) = connect(&mut p);
    let server = p.server;

    // Three groups on one track, one on another, all delivered.
    for (track, groups) in [(5u64, 3u64), (9, 1)] {
        for _ in 0..groups {
            let now = p.net.now();
            p.net
                .driver(server)
                .video_new_group(now, client_peer, track);
            let _ = p
                .net
                .driver(server)
                .send_video(now, client_peer, track, &[7; 500]);
            p.net.step(Duration::from_millis(50));
            let now = p.net.now();
            p.net
                .driver(server)
                .video_finish_group(now, client_peer, track);
            p.net.step(Duration::from_millis(50));
        }
    }

    let driver = p.net.driver(server);
    let five = driver.track_stats(client_peer, 5).expect("peer");
    let nine = driver.track_stats(client_peer, 9).expect("peer");
    assert_eq!((five.groups_started, five.groups_reset), (3, 0));
    assert_eq!((nine.groups_started, nine.groups_reset), (1, 0));
    let link = driver.stats(client_peer).expect("stats");
    assert_eq!(link.groups_started, 4);
    // A track that never opened reads zero; an unknown peer has none.
    assert_eq!(
        driver.track_stats(client_peer, 1234),
        Some(TrackStats::default())
    );
    assert_eq!(driver.track_stats(0xdead, 5), None);

    // Forgetting a track drops its figures and leaves the other's.
    driver.video_forget_track(client_peer, 5);
    assert_eq!(
        driver.track_stats(client_peer, 5),
        Some(TrackStats::default())
    );
    assert_eq!(driver.track_stats(client_peer, 9), Some(nine));
}

#[test]
fn behind_viewer_loses_groups_while_voice_keeps_flowing() {
    // Server → client at 0.5 Mbit/s, video offered at ~8 Mbit/s.
    let mut p = pair(6, LinkSpec::clean(Duration::from_millis(20)));
    p.net.set_link_one_way(
        p.server,
        p.client,
        LinkSpec::clean(Duration::from_millis(20)).with_bandwidth(500_000, 100),
    );
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let start = p.net.now();
    let mut voice_sent = 0u32;
    let mut voice_got = 0u32;
    let mut items = Vec::new();
    let mut next_group = start;
    let mut next_voice = start;
    let mut next_chunk = start;
    let mut chunks_left = 0u32;
    while p.net.now() < start + Duration::from_secs(6) {
        let now = p.net.now();
        if now >= next_group {
            next_group = now + Duration::from_millis(100);
            p.net.driver(server).video_new_group(now, client_peer, 77);
            chunks_left = 100; // 100 KB per group ≈ 8 Mbit/s
        }
        // Spread the group's chunks over the 100 ms.
        if chunks_left > 0 && now >= next_chunk {
            next_chunk = now + Duration::from_millis(1);
            chunks_left -= 1;
            let _ = p
                .net
                .driver(server)
                .send_video(now, client_peer, 77, &[1; 1000]);
        }
        if now >= next_voice {
            next_voice = now + Duration::from_millis(20);
            voice_sent += 1;
            p.net
                .driver(server)
                .send(now, client_peer, Lane::Voice, &[2; 80])
                .expect("voice");
        }
        p.net.step(Duration::from_millis(1));
        while p
            .net
            .driver(client)
            .recv(server_peer, Lane::Voice)
            .is_some()
        {
            voice_got += 1;
        }
        while let Some((track, it)) = p.net.driver(client).recv_video(server_peer) {
            assert_eq!(track, 77);
            items.push(it);
        }
    }

    let stats = p.net.driver(server).stats(client_peer).expect("stats");
    assert!(
        stats.groups_reset > 10,
        "server should abandon groups: {stats:?}"
    );
    // The one track carried every group, so its counters are the
    // connection's.
    let track = p
        .net
        .driver(server)
        .track_stats(client_peer, 77)
        .expect("track stats");
    assert_eq!(track.groups_started, stats.groups_started);
    assert_eq!(track.groups_reset, stats.groups_reset);
    // Voice rode ahead of the bulk: the bottleneck queue is 100 ms
    // deep, so the only voice loss is queue overflow.
    let voice_pct = f64::from(voice_got) * 100.0 / f64::from(voice_sent);
    assert!(
        voice_pct > 80.0,
        "voice delivery {voice_pct:.1}% ({voice_got}/{voice_sent})"
    );
    // Every group the client saw either ended or was reset; chunks of
    // an abandoned group never masquerade as a whole.
    let mut open = false;
    for it in &items {
        match it {
            VideoItem::GroupStart => {
                assert!(!open, "nested group start");
                open = true;
            }
            VideoItem::Chunk(_) => assert!(open, "chunk outside a group"),
            VideoItem::GroupEnd | VideoItem::GroupReset => {
                assert!(open, "close without start");
                open = false;
            }
        }
    }
    let resets = items
        .iter()
        .filter(|i| **i == VideoItem::GroupReset)
        .count();
    assert!(
        resets > 0,
        "client should see resets: {} items",
        items.len()
    );
}

#[test]
fn media_upload_is_paced_delivered_in_order_and_reports_progress() {
    let mut p = pair(7, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let frames: Vec<Bytes> = (0..50u8)
        .map(|i| Bytes::from(vec![i; protocol::IMAGE_CHUNK_BYTES]))
        .collect();
    let now = p.net.now();
    let upload = p
        .net
        .driver(client)
        .send_media(now, server_peer, frames.clone())
        .expect("upload");
    let (written, total) = p
        .net
        .driver(client)
        .upload_progress(server_peer, upload)
        .expect("in progress");
    assert!(
        written < total,
        "burst allowance should not cover the upload"
    );

    let start = p.net.now();
    let mut got = Vec::new();
    let mut progressed = false;
    let done = p.net.run_until(Duration::from_secs(10), TICK, |net| {
        if let Some((w, t)) = net.driver(client).upload_progress(server_peer, upload) {
            progressed |= w > 0 && w < t;
        }
        while let Some(f) = net.driver(server).recv(client_peer, Lane::Media) {
            got.push(f);
        }
        got.len() == 50
    });
    assert!(done, "got {} frames", got.len());
    assert_eq!(got, frames);
    assert!(progressed, "progress never reported a partial upload");
    // 800 KiB at 512 KiB/s: no faster than ~1.5 s.
    let took = p.net.now() - start;
    assert!(took >= Duration::from_millis(1400), "took {took:?}");
    // The upload is retired once finished.
    p.net.run_for(Duration::from_millis(500), TICK);
    assert_eq!(
        p.net.driver(client).upload_progress(server_peer, upload),
        None
    );
}

#[test]
fn trust_on_first_use_reports_the_fingerprint_then_pins() {
    let mut p = pair_with(8, LinkSpec::clean(Duration::from_millis(10)), |_| {
        Driver::new_client(None, Budget::client()).expect("client")
    });
    let expected = p.identity.fingerprint();
    connect(&mut p);
    let events = drain_client_events(&mut p);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Connected {
                identity: Some(Observation::Accepted { fingerprint, trust: Trust::FirstUse }),
                ..
            } if *fingerprint == expected
        )),
        "{events:?}"
    );
}

#[test]
fn pinned_client_is_accepted_and_reports_the_pin() {
    let mut p = pair(9, LinkSpec::clean(Duration::from_millis(10)));
    let expected = p.identity.fingerprint();
    connect(&mut p);
    let events = drain_client_events(&mut p);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Connected {
            identity: Some(Observation::Accepted { fingerprint, trust: Trust::Pinned }),
            ..
        } if *fingerprint == expected
    )));
}

#[test]
fn changed_identity_is_refused_before_any_application_data() {
    let stale_pin = Fingerprint([9; 32]);
    let mut p = pair_with(10, LinkSpec::clean(Duration::from_millis(10)), |_| {
        Driver::new_client(Some(stale_pin), Budget::client()).expect("client")
    });
    let observed = p.identity.fingerprint();
    let now = p.net.now();
    let (server, client) = (p.server, p.client);
    let server_peer = p
        .net
        .driver(client)
        .connect(now, addr(SERVER_ADDR))
        .expect("connect");
    let mut reason = None;
    let ok = p.net.run_until(Duration::from_secs(5), TICK, |net| {
        for ev in net.driver(client).drain_events() {
            if let Event::Disconnected { reason: r, .. } = ev {
                reason = Some(r);
            }
        }
        reason.is_some()
    });
    assert!(ok, "client never gave up");
    assert_eq!(
        reason,
        Some(DisconnectReason::IdentityChanged {
            expected: stale_pin,
            observed
        })
    );
    assert!(!p.net.driver(client).is_connected(server_peer));
    // The server never saw an application-level connection.
    let server_events: Vec<Event> = p.net.driver(server).drain_events().collect();
    assert!(
        !server_events
            .iter()
            .any(|e| matches!(e, Event::Connected { .. })),
        "{server_events:?}"
    );
}

#[test]
fn rotated_key_is_accepted_through_the_successor_proof() {
    init_log();
    let old = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("keygen");
    let old_fp = Fingerprint::of_spki(&old.subject_public_key_info());
    let new_identity = ServerIdentity::from_key(
        rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("keygen"),
        vec![old],
    )
    .expect("identity");
    let new_fp = new_identity.fingerprint();
    let server_driver = Driver::new_server(&new_identity, 8, Budget::server()).expect("server");
    let client_driver = Driver::new_client(Some(old_fp), Budget::client()).expect("client");
    let mut net = SimNet::new(11);
    let server = net.add_node(server_driver, addr(SERVER_ADDR));
    let client = net.add_node(client_driver, addr(CLIENT_ADDR));
    net.set_link(server, client, LinkSpec::clean(Duration::from_millis(10)));
    let mut p = Pair {
        net,
        server,
        client,
        identity: new_identity,
    };
    connect(&mut p);
    let events = drain_client_events(&mut p);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Connected {
                identity: Some(Observation::Accepted {
                    fingerprint,
                    trust: Trust::Successor { from }
                }),
                ..
            } if *fingerprint == new_fp && *from == old_fp
        )),
        "{events:?}"
    );
}

#[test]
fn alpn_mismatch_is_reported_as_an_outdated_client() {
    let mut p = pair_with(12, LinkSpec::clean(Duration::from_millis(10)), |id| {
        Driver::new_client_with_alpn(Some(id.fingerprint()), Budget::client(), b"baucord/0")
            .expect("client")
    });
    let now = p.net.now();
    let client = p.client;
    p.net
        .driver(client)
        .connect(now, addr(SERVER_ADDR))
        .expect("connect");
    let mut reason = None;
    let ok = p.net.run_until(Duration::from_secs(5), TICK, |net| {
        for ev in net.driver(client).drain_events() {
            if let Event::Disconnected { reason: r, .. } = ev {
                reason = Some(r);
            }
        }
        reason.is_some()
    });
    assert!(ok);
    assert_eq!(reason, Some(DisconnectReason::AlpnMismatch));
    assert_eq!(
        DisconnectReason::AlpnMismatch.to_string(),
        protocol::OUTDATED_MESSAGE
    );
}

#[test]
fn a_dead_peer_is_detected_by_the_idle_timeout() {
    let mut p = pair(13, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    // Pull the cable.
    p.net.set_link(
        server,
        client,
        LinkSpec::clean(Duration::from_millis(10)).with_loss(1.0),
    );
    let cut_at = p.net.now();
    let mut server_saw = None;
    let mut client_saw = None;
    let (mut server_at, mut client_at) = (None, None);
    let ok = p
        .net
        .run_until(Duration::from_secs(50), Duration::from_millis(10), |net| {
            let now = net.now();
            for ev in net.driver(server).drain_events() {
                if let Event::Disconnected { peer, reason } = ev
                    && peer == client_peer
                {
                    server_saw = Some(reason);
                    server_at = Some(now);
                }
            }
            for ev in net.driver(client).drain_events() {
                if let Event::Disconnected { peer, reason } = ev
                    && peer == server_peer
                {
                    client_saw = Some(reason);
                    client_at = Some(now);
                }
            }
            server_saw.is_some() && client_saw.is_some()
        });
    assert!(ok, "server {server_saw:?} client {client_saw:?}");
    assert_eq!(server_saw, Some(DisconnectReason::TimedOut));
    assert_eq!(client_saw, Some(DisconnectReason::TimedOut));
    assert!(server_saw
        .as_ref()
        .is_some_and(DisconnectReason::is_transient));
    // The server hears nothing for the idle timeout. The client's
    // timer starts again at the keep-alive it sends into the dead link
    // (QUIC restarts it on the first packet sent after the last one
    // received), so a client with nothing else to say takes the
    // keep-alive interval longer — the price of the 5 s keep-alive
    // (review P2-49); in a voice room it is sending all the time.
    let idle = Budget::client().max_idle_timeout;
    let slack = Duration::from_secs(5);
    let server_took = server_at.expect("seen") - cut_at;
    let client_took = client_at.expect("seen") - cut_at;
    assert!(
        server_took < idle + slack,
        "the server took {server_took:?}"
    );
    assert!(
        client_took < idle + crate::config::CLIENT_KEEP_ALIVE + slack,
        "the client took {client_took:?}"
    );
    // Both tables are eventually empty.
    p.net
        .run_for(Duration::from_secs(5), Duration::from_millis(10));
    assert_eq!(p.net.driver(server).peer_count(), 0);
    assert_eq!(p.net.driver(client).peer_count(), 0);
}

#[test]
fn a_blackout_shorter_than_the_idle_timeout_is_survived() {
    let mut p = pair(21, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    // 20 s of total loss, then the link returns.
    p.net.set_link(
        server,
        client,
        LinkSpec::clean(Duration::from_millis(10)).with_loss(1.0),
    );
    p.net
        .run_for(Duration::from_secs(20), Duration::from_millis(10));
    p.net
        .set_link(server, client, LinkSpec::clean(Duration::from_millis(10)));
    let now = p.net.now();
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Control, b"still here")
        .expect("send");
    let mut got = None;
    let ok = p.net.run_until(Duration::from_secs(5), TICK, |net| {
        got = net.driver(server).recv(client_peer, Lane::Control);
        got.is_some()
    });
    assert!(ok, "session should survive a 20 s blackout");
    assert!(p.net.driver(server).is_connected(client_peer));
    assert!(p.net.driver(client).is_connected(server_peer));
    let events: Vec<Event> = p.net.driver(client).drain_events().collect();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Disconnected { .. })),
        "{events:?}"
    );
}

#[test]
fn address_change_migrates_the_connection() {
    let mut p = pair(14, LinkSpec::clean(Duration::from_millis(15)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    let old = p.net.driver(server).remote_address(client_peer);
    assert_eq!(old, Some(addr(CLIENT_ADDR)));

    let new_addr = addr("10.0.0.99:6000");
    p.net.set_addr(client, new_addr);
    let now = p.net.now();
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Control, b"still here")
        .expect("send");
    let mut got = None;
    let ok = p.net.run_until(Duration::from_secs(3), TICK, |net| {
        got = net.driver(server).recv(client_peer, Lane::Control);
        got.is_some()
    });
    assert!(ok);
    assert_eq!(got.as_deref(), Some(&b"still here"[..]));
    p.net.run_for(Duration::from_millis(500), TICK);
    assert_eq!(
        p.net.driver(server).remote_address(client_peer),
        Some(new_addr)
    );
    assert!(p.net.driver(server).is_connected(client_peer));
    assert!(p.net.driver(client).is_connected(server_peer));
}

#[test]
fn a_full_server_refuses_new_connections() {
    init_log();
    let identity = ServerIdentity::generate();
    let server_driver = Driver::new_server(&identity, 1, Budget::server()).expect("server");
    let mut net = SimNet::new(15);
    let server = net.add_node(server_driver, addr(SERVER_ADDR));
    let c1 = net.add_node(
        Driver::new_client(Some(identity.fingerprint()), Budget::client()).expect("c1"),
        addr(CLIENT_ADDR),
    );
    let c2 = net.add_node(
        Driver::new_client(Some(identity.fingerprint()), Budget::client()).expect("c2"),
        addr("10.0.0.3:5000"),
    );
    net.set_link(server, c1, LinkSpec::clean(Duration::from_millis(10)));
    net.set_link(server, c2, LinkSpec::clean(Duration::from_millis(10)));
    let now = net.now();
    let p1 = net
        .driver(c1)
        .connect(now, addr(SERVER_ADDR))
        .expect("connect 1");
    assert!(net.run_until(Duration::from_secs(5), TICK, |net| {
        net.driver(c1).is_connected(p1)
    }));
    let now = net.now();
    let p2 = net
        .driver(c2)
        .connect(now, addr(SERVER_ADDR))
        .expect("connect 2");
    let mut refused = false;
    let ok = net.run_until(Duration::from_secs(5), TICK, |net| {
        for ev in net.driver(c2).drain_events() {
            if let Event::Disconnected { peer, .. } = ev
                && peer == p2
            {
                refused = true;
            }
        }
        refused
    });
    assert!(ok, "second client never gave up");
    assert!(!net.driver(c2).is_connected(p2));
    assert_eq!(net.driver(server).refused_count(), 1);
    assert_eq!(net.driver(server).peer_count(), 1);
}

/// A server with a waiting room: connections the application has not
/// admitted sit there, capped per address; once the room is half full
/// a newcomer proves its address first; an admitted connection gives
/// its seat back.
#[test]
fn a_waiting_room_holds_the_unadmitted_and_no_more() {
    init_log();
    let identity = ServerIdentity::generate();
    let server_driver = Driver::new_server(&identity, 1, Budget::server())
        .expect("server driver")
        .with_waiting_room(WaitingRoom {
            seats: 4,
            per_address: 2,
        });
    let mut net = SimNet::new(17);
    let server = net.add_node(server_driver, addr(SERVER_ADDR));
    let dial = |net: &mut SimNet, from: &str| {
        let node = net.add_node(
            Driver::new_client(Some(identity.fingerprint()), Budget::client()).expect("client"),
            addr(from),
        );
        net.set_link(server, node, LinkSpec::clean(Duration::from_millis(10)));
        let now = net.now();
        let peer = net
            .driver(node)
            .connect(now, addr(SERVER_ADDR))
            .expect("connect");
        // Long enough for a handshake, a Retry round trip included.
        net.run_until(Duration::from_secs(2), TICK, |net| {
            net.driver(node).is_connected(peer)
        })
    };

    // Two from one address fit; the third from it is refused, though
    // the room has seats left.
    assert!(dial(&mut net, "10.0.0.2:5000"));
    assert!(dial(&mut net, "10.0.0.2:5001"));
    assert!(!dial(&mut net, "10.0.0.2:5002"));
    assert_eq!(net.driver(server).refused_count(), 1);
    assert_eq!(net.driver(server).retried_count(), 0);

    // Half full: the next address is asked to prove itself, and gets in.
    assert!(dial(&mut net, "10.0.0.3:5000"));
    assert_eq!(net.driver(server).retried_count(), 1);
    assert!(dial(&mut net, "10.0.0.4:5000"));
    // Full: nobody else, from anywhere.
    assert!(!dial(&mut net, "10.0.0.5:5000"));
    assert_eq!(net.driver(server).peer_count(), 4);

    // The application admits one: a seat opens, beyond `max_peers`
    // for those still waiting.
    let admitted = net
        .driver(server)
        .drain_events()
        .find_map(|ev| match ev {
            Event::Connected { peer, .. } => Some(peer),
            _ => None,
        })
        .expect("a connected peer");
    net.driver(server).admit(admitted);
    assert!(dial(&mut net, "10.0.0.6:5000"));
    assert_eq!(net.driver(server).peer_count(), 5);
}

/// A connection whose handshake never finishes gives its seat back
/// after the handshake deadline, not after quinn's 30 s idle timeout.
#[test]
fn a_handshake_that_never_finishes_leaves_the_waiting_room() {
    init_log();
    let identity = ServerIdentity::generate();
    let server_driver = Driver::new_server(&identity, 1, Budget::server())
        .expect("server driver")
        .with_waiting_room(WaitingRoom {
            seats: 4,
            per_address: 2,
        });
    let mut net = SimNet::new(23);
    let server = net.add_node(server_driver, addr(SERVER_ADDR));
    let node = net.add_node(
        Driver::new_client(Some(identity.fingerprint()), Budget::client()).expect("client"),
        addr("10.0.0.9:5000"),
    );
    // The client's first packets arrive; nothing comes back to it.
    net.set_link_one_way(node, server, LinkSpec::clean(Duration::from_millis(10)));
    let mut blackhole = LinkSpec::clean(Duration::from_millis(10));
    blackhole.loss = 1.0;
    net.set_link_one_way(server, node, blackhole);
    let now = net.now();
    net.driver(node)
        .connect(now, addr(SERVER_ADDR))
        .expect("connect");
    net.run_for(Duration::from_secs(1), TICK);
    assert_eq!(net.driver(server).peer_count(), 1, "the seat is taken");
    let freed = net.run_until(Duration::from_secs(12), TICK, |net| {
        net.driver(server).peer_count() == 0
    });
    assert!(freed, "the half-open connection kept its seat");
}

#[test]
fn close_after_delivers_the_last_frame_first() {
    let mut p = pair(16, LinkSpec::clean(Duration::from_millis(20)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    let now = p.net.now();
    p.net
        .driver(server)
        .send(now, client_peer, Lane::Control, b"refused")
        .expect("send");
    p.net
        .driver(server)
        .close_after(now, client_peer, Duration::from_millis(300));
    let mut frame = None;
    let mut reason = None;
    let ok = p.net.run_until(Duration::from_secs(3), TICK, |net| {
        if frame.is_none() {
            frame = net.driver(client).recv(server_peer, Lane::Control);
        }
        for ev in net.driver(client).drain_events() {
            if let Event::Disconnected { reason: r, .. } = ev {
                reason = Some(r);
            }
        }
        frame.is_some() && reason.is_some()
    });
    assert!(ok, "frame {frame:?} reason {reason:?}");
    assert_eq!(frame.as_deref(), Some(&b"refused"[..]));
    assert!(matches!(
        reason,
        Some(DisconnectReason::ApplicationClosed { .. })
    ));
}

#[test]
fn sustained_loss_keeps_control_reliable_and_voice_lossy_only_as_the_link_is() {
    let link = LinkSpec::clean(Duration::from_millis(40))
        .with_loss(0.05)
        .with_jitter(Duration::from_millis(10));
    let mut p = pair(17, link);
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let start = p.net.now();
    let mut control_sent = 0u32;
    let mut voice_sent = 0u32;
    let mut next_control = start;
    let mut next_voice = start;
    let mut control_got = 0u32;
    let mut voice_got = 0u32;
    while p.net.now() < start + Duration::from_secs(10) {
        let now = p.net.now();
        if now >= next_control {
            next_control = now + Duration::from_millis(250);
            control_sent += 1;
            p.net
                .driver(client)
                .send(now, server_peer, Lane::Control, &[0; 64])
                .expect("control");
        }
        if now >= next_voice {
            next_voice = now + Duration::from_millis(20);
            voice_sent += 1;
            p.net
                .driver(client)
                .send(now, server_peer, Lane::Voice, &[1; 80])
                .expect("voice");
        }
        p.net.step(Duration::from_millis(1));
        while p
            .net
            .driver(server)
            .recv(client_peer, Lane::Control)
            .is_some()
        {
            control_got += 1;
        }
        while p
            .net
            .driver(server)
            .recv(client_peer, Lane::Voice)
            .is_some()
        {
            voice_got += 1;
        }
    }
    p.net.run_for(Duration::from_secs(2), TICK);
    while p
        .net
        .driver(server)
        .recv(client_peer, Lane::Control)
        .is_some()
    {
        control_got += 1;
    }
    assert_eq!(control_got, control_sent, "control must be reliable");
    let voice_loss = 100.0 - f64::from(voice_got) * 100.0 / f64::from(voice_sent);
    assert!(
        (2.0..=9.0).contains(&voice_loss),
        "voice loss {voice_loss:.1}% ({voice_got}/{voice_sent})"
    );
    assert!(p.net.driver(server).is_connected(client_peer));
    let stats = p.net.driver(server).stats(client_peer).expect("stats");
    assert!(stats.rtt >= Duration::from_millis(70), "{stats:?}");
}

#[test]
fn exporter_is_identical_on_both_ends() {
    let mut p = pair(18, LinkSpec::clean(Duration::from_millis(5)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    p.net
        .driver(client)
        .export_keying_material(server_peer, b"baucord-client-auth", b"", &mut a)
        .expect("client export");
    p.net
        .driver(server)
        .export_keying_material(client_peer, b"baucord-client-auth", b"", &mut b)
        .expect("server export");
    assert_eq!(a, b);
    assert_ne!(a, [0u8; 32]);
}

#[test]
fn stats_track_traffic() {
    let mut p = pair(19, LinkSpec::clean(Duration::from_millis(5)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    let before = p.net.driver(server).stats(client_peer).expect("stats");
    let now = p.net.now();
    for _ in 0..20 {
        p.net
            .driver(client)
            .send(now, server_peer, Lane::Voice, &[1; 80])
            .expect("voice");
    }
    p.net.run_for(Duration::from_millis(300), TICK);
    let after = p.net.driver(server).stats(client_peer).expect("stats");
    assert!(after.bytes_rx > before.bytes_rx);
    assert!(after.datagrams_rx > before.datagrams_rx);
    assert!((after.path_loss_pct_since(&before) - 0.0).abs() < f64::EPSILON);
    let _ = Instant::now(); // keep the import honest for future timing asserts
}

/// Run on the loops' schedule (review P1-2): straight from one event to
/// the next — a datagram landing, a driver's deadline — never on a fixed
/// tick, until `done` or `timeout` of simulated time. Returns whether
/// `done` hit, and the wakes it took. A deadline whose work never
/// clears spins on zero-length steps, and fails here.
fn run_on_events(
    net: &mut SimNet,
    timeout: Duration,
    mut done: impl FnMut(&mut SimNet) -> bool,
) -> (bool, u64) {
    let end = net.now() + timeout;
    let mut wakes = 0;
    let mut zero_steps = 0;
    while net.now() < end {
        if done(net) {
            return (true, wakes);
        }
        let dt = net.step_to_next_event(end - net.now());
        wakes += 1;
        zero_steps = if dt.is_zero() { zero_steps + 1 } else { 0 };
        assert!(
            zero_steps < 50,
            "spinning on a deadline nothing clears, at {:?}",
            net.now()
        );
    }
    (done(net), wakes)
}

/// What lets the loops sleep (review P1-2): an idle connection is due
/// only at its next quinn timer — the client's keep-alive, the server's
/// idle timeout — never a millisecond away.
#[test]
fn an_idle_connection_is_due_only_at_its_next_timer() {
    let mut p = pair(23, LinkSpec::clean(Duration::from_millis(10)));
    connect(&mut p);
    let (server, client) = (p.server, p.client);
    p.net.run_for(Duration::from_millis(300), TICK);
    let now = p.net.now();
    for (who, node) in [("client", client), ("server", server)] {
        let due = p
            .net
            .driver(node)
            .next_deadline()
            .expect("a live connection has timers");
        assert!(
            due >= now + Duration::from_millis(100),
            "the idle {who} is due in {:?}",
            due.saturating_duration_since(now)
        );
    }
    // On the loops' schedule two quiet seconds are a handful of wakes:
    // a keep-alive and its ack.
    let (_, wakes) = run_on_events(&mut p.net, Duration::from_secs(2), |_| false);
    assert!(wakes <= 20, "{wakes} wakes in two idle seconds");
}

/// The client's keep-alive is five seconds, not two (review P2-49). An
/// idle connection — nothing sent but the keep-alives — must still
/// outlive a link losing four packets in ten, both ways: inside the
/// 30 s idle timeout that is five pings and their retransmissions.
#[test]
fn an_idle_connection_survives_heavy_loss_on_the_slow_keep_alive() {
    assert_eq!(
        Budget::client().keep_alive_interval,
        Some(crate::config::CLIENT_KEEP_ALIVE)
    );
    for seed in [31, 32, 33] {
        let mut p = pair(seed, LinkSpec::clean(Duration::from_millis(40)));
        let (server_peer, client_peer) = connect(&mut p);
        let (server, client) = (p.server, p.client);
        p.net.set_link(
            server,
            client,
            LinkSpec::clean(Duration::from_millis(40)).with_loss(0.4),
        );
        let mut lost = false;
        run_on_events(&mut p.net, Duration::from_mins(20), |net| {
            for node in [server, client] {
                for ev in net.driver(node).drain_events() {
                    lost |= matches!(ev, Event::Disconnected { .. });
                }
            }
            lost
        });
        assert!(!lost, "seed {seed}: the idle connection was dropped");
        assert!(p.net.driver(client).is_connected(server_peer));
        assert!(p.net.driver(server).is_connected(client_peer));
    }
}

/// The pacer's refill is a deadline: a loop that only wakes for events
/// still moves a paced upload, with about one wake per chunk beyond the
/// datagrams themselves, and the connection is idle again once the
/// upload is acknowledged and retired.
#[test]
fn a_paced_upload_completes_on_the_loops_schedule() {
    let mut p = pair(24, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    p.net.run_for(Duration::from_millis(300), TICK);

    let frames: Vec<Bytes> = (0..20u8)
        .map(|i| Bytes::from(vec![i; protocol::IMAGE_CHUNK_BYTES]))
        .collect();
    let now = p.net.now();
    let upload = p
        .net
        .driver(client)
        .send_media(now, server_peer, frames.clone())
        .expect("upload");
    let delivered_before = p.net.delivered;
    let mut got = Vec::new();
    let (retired, wakes) = run_on_events(&mut p.net, Duration::from_secs(10), |net| {
        while let Some(f) = net.driver(server).recv(client_peer, Lane::Media) {
            got.push(f);
        }
        net.driver(client)
            .upload_progress(server_peer, upload)
            .is_none()
    });
    assert!(retired, "the upload stalled after {} frames", got.len());
    assert_eq!(got, frames);
    // 320 KiB at 512 KiB/s is ~600 ms: a 1 ms tick was 600 wakes on top
    // of the datagrams.
    let arrivals = p.net.delivered - delivered_before;
    let timer_wakes = wakes.saturating_sub(arrivals);
    assert!(
        timer_wakes < 100,
        "{timer_wakes} wakes besides {arrivals} arrivals"
    );
    // The last acks cross (a probe timer can still be armed the moment
    // the upload retires — about one run in twenty); then nothing of
    // the upload is left on a clock.
    run_on_events(&mut p.net, Duration::from_millis(300), |_| false);
    let now = p.net.now();
    let due = p.net.driver(client).next_deadline().expect("timers");
    assert!(
        due >= now + Duration::from_millis(100),
        "still due in {:?} after the upload",
        due.saturating_duration_since(now)
    );
}

/// A sharer whose bytes are all acknowledged costs no ticks: an open
/// group with nothing queued has no deadline (the network thread used
/// to run at 1 kHz for the whole length of a share).
#[test]
fn an_open_group_with_nothing_queued_is_not_due() {
    let mut p = pair(25, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    let now = p.net.now();
    p.net
        .driver(client)
        .video_new_group(now, server_peer, OWN_TRACK);
    for i in 0..5u8 {
        p.net
            .driver(client)
            .send(now, server_peer, Lane::Video, &[i; 1000])
            .expect("chunk");
    }
    let mut items = 0;
    let (got, _) = run_on_events(&mut p.net, Duration::from_secs(2), |net| {
        while net.driver(server).recv_video(client_peer).is_some() {
            items += 1;
        }
        items == 6
    });
    assert!(got, "{items} video items arrived");
    run_on_events(&mut p.net, Duration::from_millis(300), |_| false);
    assert!(p
        .net
        .driver(client)
        .video_group_open(server_peer, OWN_TRACK));
    let now = p.net.now();
    let due = p.net.driver(client).next_deadline().expect("timers");
    assert!(
        due >= now + Duration::from_millis(100),
        "an idle open group is due in {:?}",
        due.saturating_duration_since(now)
    );
}

/// With the link dark — no ack, no new frame, nothing else happening —
/// the video deadlines alone wake the loop: a group's queued bytes are
/// abandoned 500 ms after they queued, an unacknowledged finish 500 ms
/// plus two RTTs after it.
#[test]
fn stalled_groups_are_reset_at_their_deadlines_on_the_loops_schedule() {
    let mut p = pair(26, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, _) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    p.net.set_link(
        server,
        client,
        LinkSpec::clean(Duration::from_millis(10)).with_loss(1.0),
    );
    let start = p.net.now();
    // Track 1: a small group, written and finished at once.
    p.net.driver(client).video_new_group(start, server_peer, 1);
    p.net
        .driver(client)
        .send_video(start, server_peer, 1, &[1; 1000])
        .expect("chunk");
    p.net
        .driver(client)
        .video_finish_group(start, server_peer, 1);
    // Track 0: more than the 512 KiB send window, so the rest waits in
    // the driver with its timestamp.
    p.net
        .driver(client)
        .video_new_group(start, server_peer, OWN_TRACK);
    for _ in 0..700 {
        p.net
            .driver(client)
            .send(start, server_peer, Lane::Video, &[7; 1000])
            .expect("chunk");
    }

    // The reset happens in the wake at the last instant `done` said no.
    let mut woke_at = start;
    let (reset, _) = run_on_events(&mut p.net, Duration::from_secs(2), |net| {
        let hit = net
            .driver(client)
            .stats(server_peer)
            .is_some_and(|s| s.groups_reset_deadline > 0);
        if !hit {
            woke_at = net.now();
        }
        hit
    });
    let took = woke_at - start;
    assert!(reset, "the queued group was never abandoned");
    assert!(
        took >= VIDEO_GROUP_DEADLINE && took < VIDEO_GROUP_DEADLINE + Duration::from_millis(5),
        "abandoned after {took:?}"
    );

    let (reset, _) = run_on_events(&mut p.net, Duration::from_secs(2), |net| {
        let hit = net
            .driver(client)
            .stats(server_peer)
            .is_some_and(|s| s.groups_reset_unacked > 0);
        if !hit {
            woke_at = net.now();
        }
        hit
    });
    let took = woke_at - start;
    let rtt = p.net.driver(client).stats(server_peer).expect("stats").rtt;
    assert!(reset, "the unacknowledged group was never abandoned");
    let due = VIDEO_GROUP_DEADLINE + 2 * rtt;
    assert!(
        took >= due && took < due + Duration::from_millis(5),
        "abandoned after {took:?}, due at {due:?}"
    );
}

// ── What the simulator had never been made to do (review P3-4) ──────────

/// The server closed the session while the client could not hear it,
/// and has forgotten it by the time the link is back: the client's next
/// packet is answered with a stateless reset, and the client knows at
/// once instead of waiting out its idle timeout.
#[test]
fn a_session_the_server_forgot_is_reset_at_the_clients_next_packet() {
    let mut p = pair(41, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    p.net.driver(client).drain_events().for_each(drop);

    p.net.set_link(
        server,
        client,
        LinkSpec::clean(Duration::from_millis(10)).with_loss(1.0),
    );
    let now = p.net.now();
    p.net.driver(server).close(now, client_peer, "gone");
    // Long enough for the server to finish closing and drop the state,
    // well short of the client's own 30 s idle timeout.
    p.net
        .run_for(Duration::from_secs(5), Duration::from_millis(10));
    assert!(
        p.net.driver(client).is_connected(server_peer),
        "the client heard nothing: it still thinks it is connected"
    );
    p.net
        .set_link(server, client, LinkSpec::clean(Duration::from_millis(10)));

    let back = p.net.now();
    p.net
        .driver(client)
        .send(back, server_peer, Lane::Control, b"anyone there?")
        .expect("send");
    let mut reason = None;
    let ok = p.net.run_until(Duration::from_secs(3), TICK, |net| {
        for ev in net.driver(client).drain_events() {
            if let Event::Disconnected { reason: r, .. } = ev {
                reason = Some(r);
            }
        }
        reason.is_some()
    });
    assert!(
        ok,
        "the client should learn within seconds, not at its idle timeout"
    );
    assert_eq!(reason, Some(DisconnectReason::Reset));
}

/// A receiver that refuses a stream (`STOP_SENDING`) costs the sender
/// that group and nothing else: it is counted as reset, and the next
/// group goes through.
#[test]
fn a_refused_stream_loses_its_group_and_the_next_one_flows() {
    let mut p = pair(42, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    // A group left open, so its stream is still there to refuse.
    let now = p.net.now();
    p.net.driver(client).video_new_group(now, server_peer, 0);
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Video, &[1; 1000])
        .expect("chunk");
    p.net.run_for(Duration::from_millis(200), TICK);
    while p.net.driver(server).recv_video(client_peer).is_some() {}
    let before = p
        .net
        .driver(client)
        .track_stats(server_peer, 0)
        .expect("track");

    let now = p.net.now();
    p.net.driver(server).stop_incoming_streams(now, client_peer);
    p.net.run_for(Duration::from_millis(200), TICK);
    let after = p
        .net
        .driver(client)
        .track_stats(server_peer, 0)
        .expect("track");
    assert_eq!(after.groups_reset, before.groups_reset + 1);
    assert!(
        !p.net.driver(client).video_group_open(server_peer, 0),
        "the refused group is gone"
    );
    assert!(p.net.driver(client).is_connected(server_peer));

    // Chunks for the lost group go nowhere; the next keyframe's group
    // arrives whole.
    let now = p.net.now();
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Video, &[2; 1000])
        .expect("dropped quietly");
    p.net.driver(client).video_new_group(now, server_peer, 0);
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Video, &[3; 1000])
        .expect("chunk");
    p.net.driver(client).video_finish_group(now, server_peer, 0);
    let mut items = Vec::new();
    p.net.run_until(Duration::from_secs(2), TICK, |net| {
        while let Some((_, it)) = net.driver(server).recv_video(client_peer) {
            items.push(it);
        }
        matches!(items.last(), Some(VideoItem::GroupEnd))
    });
    assert_eq!(
        items,
        [
            VideoItem::GroupStart,
            VideoItem::Chunk(Bytes::from(vec![3; 1000])),
            VideoItem::GroupEnd
        ]
    );
}

/// More voice than a stalled link takes: the datagram queue fills, the
/// surplus is dropped and counted, the application is told once per
/// episode — and nothing panics. (quinn-proto 0.11.17's drop-oldest
/// path did, the first time a real link stalled; the driver drops the
/// newest instead.)
#[test]
fn voice_into_a_stalled_link_is_dropped_counted_and_reported_once() {
    let mut p = pair(43, LinkSpec::clean(Duration::from_millis(10)));
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);
    p.net.driver(client).drain_events().for_each(drop);

    p.net.set_link(
        server,
        client,
        LinkSpec::clean(Duration::from_millis(10)).with_loss(1.0),
    );
    // Ten seconds of voice at once: far past the client's 32 KiB.
    let now = p.net.now();
    for i in 0..500u32 {
        p.net
            .driver(client)
            .send(now, server_peer, Lane::Voice, &[(i % 251) as u8; 300])
            .expect("a full queue is not an error");
    }
    let stats = p.net.driver(client).stats(server_peer).expect("stats");
    assert!(stats.datagram_send_drops > 0, "{stats:?}");
    assert_eq!(stats.datagrams_blocked, 1, "one episode, however long");
    let blocked = p
        .net
        .driver(client)
        .drain_events()
        .filter(|e| matches!(e, Event::DatagramsBlocked { .. }))
        .count();
    assert_eq!(blocked, 1);

    // The link returns: voice flows again, and a new stall is a new
    // episode.
    p.net
        .set_link(server, client, LinkSpec::clean(Duration::from_millis(10)));
    p.net.run_for(Duration::from_secs(2), TICK);
    while p
        .net
        .driver(server)
        .recv(client_peer, Lane::Voice)
        .is_some()
    {}
    let now = p.net.now();
    p.net
        .driver(client)
        .send(now, server_peer, Lane::Voice, &[9; 100])
        .expect("voice");
    let mut got = None;
    p.net.run_until(Duration::from_secs(2), TICK, |net| {
        while let Some(d) = net.driver(server).recv(client_peer, Lane::Voice) {
            got = Some(d);
        }
        got.as_ref().is_some_and(|d| d.len() == 100)
    });
    assert_eq!(got.map(|d| d.len()), Some(100));
    assert!(p.net.driver(client).is_connected(server_peer));
}

/// Packets that overtake each other (jitter wider than the gap between
/// them) on two video tracks at once: every group of each track still
/// comes out whole and in order, and nothing of one track turns up in
/// the other.
#[test]
fn two_tracks_over_a_reordering_link_each_arrive_whole_and_in_order() {
    const GROUPS: u8 = 12;
    const TRACKS: [u64; 2] = [5, 9];
    let link = LinkSpec::clean(Duration::from_millis(10)).with_jitter(Duration::from_millis(25));
    let mut p = pair(44, link);
    let (server_peer, client_peer) = connect(&mut p);
    let (server, client) = (p.server, p.client);

    let mut seen: [Vec<VideoItem>; 2] = [Vec::new(), Vec::new()];
    let collect = |net: &mut SimNet, seen: &mut [Vec<VideoItem>; 2]| {
        while let Some((track, it)) = net.driver(client).recv_video(server_peer) {
            let slot = TRACKS
                .iter()
                .position(|t| *t == track)
                .expect("a known track");
            seen[slot].push(it);
        }
    };
    for g in 0..GROUPS {
        let now = p.net.now();
        for (slot, track) in TRACKS.into_iter().enumerate() {
            let d = p.net.driver(server);
            d.video_new_group(now, client_peer, track);
            for i in 0..3u8 {
                let byte = u8::try_from(slot).unwrap() * 100 + g * 4 + i;
                d.send_video(now, client_peer, track, &[byte; 900])
                    .expect("chunk");
            }
            d.video_finish_group(now, client_peer, track);
        }
        p.net.run_for(Duration::from_millis(60), TICK);
        collect(&mut p.net, &mut seen);
    }
    p.net.run_for(Duration::from_millis(500), TICK);
    collect(&mut p.net, &mut seen);

    for (track, items) in seen.iter().enumerate() {
        let mut expected = Vec::new();
        for g in 0..GROUPS {
            expected.push(VideoItem::GroupStart);
            for i in 0..3u8 {
                let byte = u8::try_from(track).unwrap() * 100 + g * 4 + i;
                expected.push(VideoItem::Chunk(Bytes::from(vec![byte; 900])));
            }
            expected.push(VideoItem::GroupEnd);
        }
        assert_eq!(items.len(), expected.len(), "track {track}: {items:?}");
        assert_eq!(*items, expected, "track {track}");
    }
}
