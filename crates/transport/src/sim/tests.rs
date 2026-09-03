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
    driver::{DisconnectReason, Driver, Event, PeerId, VideoItem},
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
    let ok = p
        .net
        .run_until(Duration::from_secs(40), Duration::from_millis(10), |net| {
            for ev in net.driver(server).drain_events() {
                if let Event::Disconnected { peer, reason } = ev
                    && peer == client_peer
                {
                    server_saw = Some(reason);
                }
            }
            for ev in net.driver(client).drain_events() {
                if let Event::Disconnected { peer, reason } = ev
                    && peer == server_peer
                {
                    client_saw = Some(reason);
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
    let took = p.net.now() - cut_at;
    assert!(took < Duration::from_secs(35), "took {took:?}");
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
