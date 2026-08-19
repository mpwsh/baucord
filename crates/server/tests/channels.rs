//! End-to-end channel tests: a real `ChatServer` on a localhost UDP
//! socket, driven tick-by-tick against real renet clients. Everything
//! crosses the actual wire — no mocks.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

use renet::RenetClient;
use renet_netcode::{ClientAuthentication, NetcodeClientTransport};

use protocol::{
    ChannelDenyReason, ClientMessage, DenyReason, ServerMessage, ShareScope, CONTROL_CHANNEL,
};
use server::config::{ChannelSettings, Config};
use server::server::ChatServer;

// ── Harness ─────────────────────────────────────────────────────────────

fn test_config(channels: Vec<ChannelSettings>) -> Config {
    let mut config = Config::default();
    config.server.host = "127.0.0.1".to_string();
    config.server.port = 0; // ephemeral — read back via addresses()
    config.channels = channels;
    config
}

fn open_channel(name: &str, default: bool) -> ChannelSettings {
    ChannelSettings {
        name: name.to_string(),
        default,
        password: None,
        user_limit: None,
    }
}

struct TestClient {
    client: RenetClient,
    transport: NetcodeClientTransport,
    last: Instant,
    inbox: Vec<ServerMessage>,
}

impl TestClient {
    fn connect(server_addr: SocketAddr, name: &str, id: u64) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind client socket");
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock");
        let authentication = ClientAuthentication::Unsecure {
            protocol_id: protocol::PROTOCOL_ID,
            client_id: id,
            server_addr,
            user_data: Some(protocol::encode_username(name)),
        };
        let transport =
            NetcodeClientTransport::new(now, authentication, socket).expect("client transport");
        TestClient {
            client: RenetClient::new(protocol::connection_config(
                protocol::CLIENT_BYTES_PER_TICK,
            )),
            transport,
            last: Instant::now(),
            inbox: Vec::new(),
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now - self.last;
        self.last = now;
        self.client.update(dt);
        self.transport
            .update(dt, &mut self.client)
            .expect("client transport update");
        for channel in [CONTROL_CHANNEL, protocol::MEDIA_CHANNEL] {
            while let Some(raw) = self.client.receive_message(channel) {
                if let Some(msg) = protocol::decode_server_message(&raw) {
                    self.inbox.push(msg);
                }
            }
        }
        self.transport
            .send_packets(&mut self.client)
            .expect("client send");
    }

    fn send(&mut self, msg: &ClientMessage) {
        let bytes = protocol::encode_client_message(msg).expect("encode");
        self.client.send_message(msg.channel(), bytes);
    }

    fn send_raw(&mut self, channel: u8, bytes: Vec<u8>) {
        self.client.send_message(channel, bytes);
    }

    /// Pump server + clients until `pred` finds a match in this
    /// client's inbox (returned), or panic after 5 s.
    fn wait_for<T>(
        &mut self,
        server: &mut ChatServer,
        others: &mut [&mut TestClient],
        mut pred: impl FnMut(&ServerMessage) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            server.update().expect("server tick");
            self.tick();
            for other in others.iter_mut() {
                other.tick();
            }
            if let Some(found) = self.inbox.iter().find_map(&mut pred) {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for message");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn start_server(channels: Vec<ChannelSettings>) -> (ChatServer, SocketAddr) {
    let server = ChatServer::new(test_config(channels)).expect("server start");
    let addr = server.addresses()[0];
    (server, addr)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[test]
fn init_carries_channels_and_default_membership() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Gaming".to_string(),
            default: false,
            password: Some("hunter2".to_string()),
            user_limit: Some(8),
        },
    ]);
    let mut a = TestClient::connect(addr, "alice", 1);

    let (channels, memberships, my_id) = a.wait_for(&mut server, &mut [], |m| match m {
        ServerMessage::Init {
            channels,
            memberships,
            client_id,
            ..
        } => Some((channels.clone(), memberships.clone(), *client_id)),
        _ => None,
    });

    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].name, "Lobby");
    assert!(!channels[0].has_password);
    assert!(channels[1].has_password);
    assert_eq!(channels[1].user_limit, Some(8));
    // We land in the default channel (index 0).
    assert_eq!(memberships.get(&my_id), Some(&0));
}

#[test]
fn join_password_flow_wrong_then_right() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Sekrit".to_string(),
            default: false,
            password: Some("hunter2".to_string()),
            user_limit: None,
        },
    ]);
    let mut a = TestClient::connect(addr, "alice", 1);

    // Wrong password → denied.
    a.wait_for(&mut server, &mut [], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: Some("wrong".to_string()),
    });
    a.wait_for(&mut server, &mut [], |m| {
        matches!(
            m,
            ServerMessage::JoinChannelDenied {
                channel_id: 1,
                reason: ChannelDenyReason::WrongPassword,
            }
        )
        .then_some(())
    });

    // Right password → moved, echoed back to us.
    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: Some("hunter2".to_string()),
    });
    let moved_to = a.wait_for(&mut server, &mut [], |m| match m {
        ServerMessage::UserMovedChannel { channel_id, .. } => Some(*channel_id),
        _ => None,
    });
    assert_eq!(moved_to, 1);

    // Bogus channel id → NoSuchChannel.
    a.send(&ClientMessage::JoinChannel {
        channel_id: 99,
        password: None,
    });
    a.wait_for(&mut server, &mut [], |m| {
        matches!(
            m,
            ServerMessage::JoinChannelDenied {
                reason: ChannelDenyReason::NoSuchChannel,
                ..
            }
        )
        .then_some(())
    });
}

#[test]
fn user_limit_fills_channel() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Duo".to_string(),
            default: false,
            password: None,
            user_limit: Some(1),
        },
    ]);
    let mut a = TestClient::connect(addr, "alice", 1);
    let mut b = TestClient::connect(addr, "bob", 2);

    a.wait_for(&mut server, &mut [&mut b], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });

    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    a.wait_for(&mut server, &mut [&mut b], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // Channel full for the second client.
    b.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(
            m,
            ServerMessage::JoinChannelDenied {
                channel_id: 1,
                reason: ChannelDenyReason::ChannelFull,
            }
        )
        .then_some(())
    });
}

#[test]
fn channel_scoped_share_visibility_follows_membership() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]);
    let mut sharer = TestClient::connect(addr, "sharer", 1);
    let mut viewer = TestClient::connect(addr, "viewer", 2);

    sharer.wait_for(&mut server, &mut [&mut viewer], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    viewer.wait_for(&mut server, &mut [&mut sharer], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });

    // Sharer moves to Movies and starts a channel-scoped share there.
    sharer.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    sharer.wait_for(&mut server, &mut [&mut viewer], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });
    sharer.send(&ClientMessage::ScreenShareStart {
        width: 1280,
        height: 720,
        scope: ShareScope::Channel,
    });
    // The sharer's own channel hears the announcement…
    sharer.wait_for(&mut server, &mut [&mut viewer], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { .. }).then_some(())
    });
    // …and a viewer in the Lobby must NOT: subscribing is refused.
    assert!(
        !viewer
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::ScreenShareStarted { .. })),
        "channel-scoped share leaked to another channel"
    );
    viewer.send(&ClientMessage::SubscribeScreen { target_id: 1 });
    viewer.wait_for(&mut server, &mut [&mut sharer], |m| {
        matches!(
            m,
            ServerMessage::SubscribeDenied {
                reason: DenyReason::NotInChannel,
                ..
            }
        )
        .then_some(())
    });

    // The viewer moves into Movies → snapshot announcement arrives and
    // subscribing now succeeds (no denial; keyframe machinery kicks in
    // server-side, nothing for the viewer to assert beyond the Started).
    viewer.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    viewer.wait_for(&mut server, &mut [&mut sharer], |m| {
        matches!(
            m,
            ServerMessage::ScreenShareStarted {
                client_id: 1,
                scope: ShareScope::Channel,
                ..
            }
        )
        .then_some(())
    });
    viewer.send(&ClientMessage::SubscribeScreen { target_id: 1 });

    // Sharer moves back to the Lobby → the viewer (left behind in
    // Movies) is evicted: their view of the stream ends.
    sharer.send(&ClientMessage::JoinChannel {
        channel_id: 0,
        password: None,
    });
    viewer.wait_for(&mut server, &mut [&mut sharer], |m| {
        matches!(m, ServerMessage::ScreenShareStopped { client_id: 1 }).then_some(())
    });
}

#[test]
fn config_validation_rejects_bad_channel_lists() {
    // No default.
    let config = test_config(vec![open_channel("A", false)]);
    assert!(config.validate_channels().is_err());
    // Two defaults.
    let config = test_config(vec![open_channel("A", true), open_channel("B", true)]);
    assert!(config.validate_channels().is_err());
    // Password on the default.
    let config = test_config(vec![ChannelSettings {
        name: "A".to_string(),
        default: true,
        password: Some("x".to_string()),
        user_limit: None,
    }]);
    assert!(config.validate_channels().is_err());
    // user_limit on the default.
    let config = test_config(vec![ChannelSettings {
        name: "A".to_string(),
        default: true,
        password: None,
        user_limit: Some(4),
    }]);
    assert!(config.validate_channels().is_err());
    // Duplicate names.
    let config = test_config(vec![open_channel("A", true), open_channel("A", false)]);
    assert!(config.validate_channels().is_err());
    // Missing key in YAML → single default Lobby, valid.
    let config: Config = serde_yaml::from_str("server:\n  name: test\n").expect("yaml");
    assert_eq!(config.validate_channels().expect("valid"), 0);
    assert_eq!(config.channels[0].name, "Lobby");
}

#[test]
fn hot_reload_moves_renamed_channel_members_to_default() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]);
    let mut a = TestClient::connect(addr, "alice", 1);
    a.wait_for(&mut server, &mut [], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    a.wait_for(&mut server, &mut [], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // "Movies" becomes "Cinema": same slot, new name ⇒ new channel ⇒
    // members go to the default.
    server
        .apply_channel_config(vec![open_channel("Lobby", true), open_channel("Cinema", false)])
        .expect("valid list");

    let (channels, memberships) = a.wait_for(&mut server, &mut [], |m| match m {
        ServerMessage::ChannelListUpdated {
            channels,
            memberships,
        } => Some((channels.clone(), memberships.clone())),
        _ => None,
    });
    assert_eq!(channels[1].name, "Cinema");
    assert_eq!(memberships.get(&1), Some(&0), "renamed ⇒ back to Lobby");
}

#[test]
fn hot_reload_password_change_keeps_members_but_gates_new_joins() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        open_channel("Duo", false),
    ]);
    let mut a = TestClient::connect(addr, "alice", 1);
    let mut b = TestClient::connect(addr, "bob", 2);
    a.wait_for(&mut server, &mut [&mut b], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    a.wait_for(&mut server, &mut [&mut b], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // Duo gains a password. Alice (inside) stays; Bob now needs it.
    server
        .apply_channel_config(vec![
            open_channel("Lobby", true),
            ChannelSettings {
                name: "Duo".to_string(),
                default: false,
                password: Some("sesame".to_string()),
                user_limit: None,
            },
        ])
        .expect("valid list");

    let memberships = b.wait_for(&mut server, &mut [&mut a], |m| match m {
        ServerMessage::ChannelListUpdated { memberships, .. } => Some(memberships.clone()),
        _ => None,
    });
    assert_eq!(memberships.get(&1), Some(&1), "existing member never kicked");

    b.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(
            m,
            ServerMessage::JoinChannelDenied {
                channel_id: 1,
                reason: ChannelDenyReason::WrongPassword,
            }
        )
        .then_some(())
    });
    b.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: Some("sesame".to_string()),
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(
            m,
            ServerMessage::UserMovedChannel { client_id: 2, channel_id: 1 }
        )
        .then_some(())
    });
}

#[test]
fn hot_reload_rederives_channel_scoped_share_visibility() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]);
    let mut sharer = TestClient::connect(addr, "sharer", 1);
    let mut viewer = TestClient::connect(addr, "viewer", 2);
    let mut lobbyist = TestClient::connect(addr, "lobbyist", 3);

    sharer.wait_for(&mut server, &mut [&mut viewer, &mut lobbyist], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    viewer.wait_for(&mut server, &mut [&mut sharer, &mut lobbyist], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    lobbyist.wait_for(&mut server, &mut [&mut sharer, &mut viewer], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });

    // Sharer + viewer set up a channel-scoped stream in Movies.
    for (client_id, c) in [(1u64, &mut sharer), (2u64, &mut viewer)] {
        c.send(&ClientMessage::JoinChannel {
            channel_id: 1,
            password: None,
        });
        let _ = client_id;
    }
    viewer.wait_for(&mut server, &mut [&mut sharer, &mut lobbyist], |m| {
        matches!(
            m,
            ServerMessage::UserMovedChannel { client_id: 2, channel_id: 1 }
        )
        .then_some(())
    });
    sharer.send(&ClientMessage::ScreenShareStart {
        width: 1280,
        height: 720,
        scope: ShareScope::Channel,
    });
    viewer.wait_for(&mut server, &mut [&mut sharer, &mut lobbyist], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { client_id: 1, .. }).then_some(())
    });
    viewer.send(&ClientMessage::SubscribeScreen { target_id: 1 });
    assert!(
        !lobbyist
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::ScreenShareStarted { .. })),
        "lobby must not see the Movies-scoped stream yet"
    );

    // Movies disappears: sharer AND viewer land in the Lobby together —
    // the stream follows them, the viewer keeps watching, and the
    // lobbyist (newly co-channeled) now learns about it.
    server
        .apply_channel_config(vec![open_channel("Lobby", true)])
        .expect("valid list");

    lobbyist.wait_for(&mut server, &mut [&mut sharer, &mut viewer], |m| {
        matches!(
            m,
            ServerMessage::ScreenShareStarted {
                client_id: 1,
                scope: ShareScope::Channel,
                ..
            }
        )
        .then_some(())
    });
    assert!(
        !viewer
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::ScreenShareStopped { client_id: 1 })),
        "viewer moved together with the sharer — must not be evicted"
    );
}

#[test]
fn hot_reload_rejects_invalid_lists_and_keeps_state() {
    let (mut server, addr) = start_server(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]);
    let mut a = TestClient::connect(addr, "alice", 1);
    a.wait_for(&mut server, &mut [], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    a.wait_for(&mut server, &mut [], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // No default / duplicate names: both rejected, nothing broadcast.
    assert!(server.apply_channel_config(vec![open_channel("A", false)]).is_err());
    assert!(
        server
            .apply_channel_config(vec![open_channel("A", true), open_channel("A", false)])
            .is_err()
    );

    // Membership untouched: joining Movies again is a no-op (no deny,
    // no move), proving the live list survived the bad edits.
    a.inbox.clear();
    a.send(&ClientMessage::JoinChannel {
        channel_id: 1,
        password: None,
    });
    a.send(&ClientMessage::Chat("ping".to_string()));
    a.wait_for(&mut server, &mut [], |m| {
        matches!(m, ServerMessage::Chat(c) if c.text == "ping").then_some(())
    });
    assert!(
        !a.inbox.iter().any(|m| matches!(
            m,
            ServerMessage::JoinChannelDenied { .. } | ServerMessage::UserMovedChannel { .. }
        )),
        "state changed after rejected reloads"
    );
}

#[test]
fn per_client_image_byte_budget_brakes_one_spammer() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_image_kb_per_minute = 100; // 100 KiB/min per client
    config.limits.max_image_kb_per_minute_global = 100_000; // loose
    let mut server = ChatServer::new(config).expect("server start");
    let addr = server.addresses()[0];

    let mut a = TestClient::connect(addr, "alice", 1);
    let mut b = TestClient::connect(addr, "bob", 2);
    a.wait_for(&mut server, &mut [&mut b], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });

    // Three ~40 KiB images from alice: 40+40 fits the 100 KiB window,
    // the third (120 KiB total) must be dropped. Bob's own budget is
    // untouched — his image still relays.
    for seed in 0..3u8 {
        let msg = ClientMessage::ChatImage(png_blob(seed, 40 * 1024));
        let bytes = protocol::encode_client_message(&msg).expect("encode");
        a.send_raw(msg.channel(), bytes);
    }
    let msg = ClientMessage::ChatImage(png_blob(9, 40 * 1024));
    let bytes = protocol::encode_client_message(&msg).expect("encode");
    b.send_raw(msg.channel(), bytes);

    settle(&mut server, &mut a, &mut b, Duration::from_secs(2));
    let relayed = count_images(&a);
    assert_eq!(relayed, 3, "2 of alice's 3 + bob's 1 must relay");
}

#[test]
fn global_image_byte_budget_brakes_coordinated_bursts() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_image_kb_per_minute = 100_000; // loose
    config.limits.max_image_kb_per_minute_global = 100; // 100 KiB/min room-wide
    let mut server = ChatServer::new(config).expect("server start");
    let addr = server.addresses()[0];

    let mut a = TestClient::connect(addr, "alice", 1);
    let mut b = TestClient::connect(addr, "bob", 2);
    a.wait_for(&mut server, &mut [&mut b], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    b.wait_for(&mut server, &mut [&mut a], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });

    // 40 KiB from each side, alternating: the room window admits two
    // (80 KiB) and must drop the third and fourth.
    for (i, who) in [0u8, 1, 0, 1].iter().enumerate() {
        let msg = ClientMessage::ChatImage(png_blob(u8::try_from(i).unwrap(), 40 * 1024));
        let bytes = protocol::encode_client_message(&msg).expect("encode");
        match who {
            0 => a.send_raw(msg.channel(), bytes),
            _ => b.send_raw(msg.channel(), bytes),
        }
    }

    settle(&mut server, &mut a, &mut b, Duration::from_secs(2));
    assert_eq!(count_images(&a), 2, "room budget must stop the burst at 2");
}

fn png_blob(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend(std::iter::repeat_n(seed, len.saturating_sub(v.len())));
    v
}

fn settle(server: &mut ChatServer, a: &mut TestClient, b: &mut TestClient, d: Duration) {
    let until = std::time::Instant::now() + d;
    while std::time::Instant::now() < until {
        server.update().expect("tick");
        a.tick();
        b.tick();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn count_images(c: &TestClient) -> usize {
    c.inbox
        .iter()
        .filter(|m| matches!(m, ServerMessage::ChatImage(_)))
        .count()
}

