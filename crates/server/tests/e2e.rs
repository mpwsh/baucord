//! End-to-end server tests on the in-process network: a real
//! `ChatServer` and real client `Driver`s, every message crossing the
//! simulated wire through the full QUIC stack, on a fake clock.

use std::{collections::HashMap, net::SocketAddr, time::Duration};

use protocol::{
    features, Auth, ChannelDenyReason, ClientId, ClientMessage, DenyReason, Lane, RefusedReason,
    ServerMessage, ShareScope,
};
use server::{
    config::{ChannelSettings, Config},
    server::ChatServer,
};
use transport::{
    sim::{LinkSpec, NodeId, SimNet},
    Budget, DisconnectReason, Driver, Event, Fingerprint, PeerId, ServerIdentity, VideoItem,
    OWN_TRACK,
};

const TICK: Duration = Duration::from_millis(2);
const SERVER_ADDR: &str = "10.0.0.1:5000";
const WAIT: Duration = Duration::from_secs(5);

// ── Harness ─────────────────────────────────────────────────────────────

fn test_config(channels: Vec<ChannelSettings>) -> Config {
    let mut config = Config {
        channels,
        ..Config::default()
    };
    config.logging.link_stats_interval_secs = 0;
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

struct World {
    net: SimNet,
    server: NodeId,
    fingerprint: Fingerprint,
    next_client: u8,
}

struct TestClient {
    node: NodeId,
    /// The server, as this client's driver names it.
    peer: PeerId,
    /// The id the server assigned to this client.
    id: ClientId,
    inbox: Vec<ServerMessage>,
    events: Vec<Event>,
}

impl World {
    fn new(config: Config) -> Self {
        let _ = env_logger::builder().is_test(true).try_init();
        let identity = ServerIdentity::generate();
        let fingerprint = identity.fingerprint();
        let server = ChatServer::new(config, &identity).expect("server start");
        let mut net = SimNet::new(0xBA0C);
        let server = net.add_node(server, SERVER_ADDR.parse().expect("addr"));
        World {
            net,
            server,
            fingerprint,
            next_client: 2,
        }
    }

    fn server(&mut self) -> &mut ChatServer {
        self.net
            .host_mut::<ChatServer>(self.server)
            .expect("server node")
    }

    /// One tick: move datagrams, run the server, collect what the
    /// clients received.
    fn pump(&mut self, clients: &mut [&mut TestClient]) {
        self.net.step(TICK);
        let now = self.net.now();
        self.server().update(now);
        for c in clients.iter_mut() {
            let driver = self.net.driver(c.node);
            c.events.extend(driver.drain_events());
            for lane in [Lane::Control, Lane::Signal, Lane::Media] {
                while let Some(raw) = driver.recv(c.peer, lane) {
                    if let Some(msg) = protocol::decode_server_message(&raw) {
                        c.inbox.push(msg);
                    }
                }
            }
        }
    }

    fn settle(&mut self, clients: &mut [&mut TestClient], d: Duration) {
        let until = self.net.now() + d;
        while self.net.now() < until {
            self.pump(clients);
        }
    }

    /// Open a transport connection (no Hello yet) and wait for the
    /// handshake.
    fn open(&mut self) -> TestClient {
        let mut client = self.open_attempt();
        let deadline = self.net.now() + WAIT;
        while !self.net.driver(client.node).is_connected(client.peer) {
            assert!(self.net.now() < deadline, "handshake timed out");
            self.pump(&mut [&mut client]);
        }
        client
    }

    /// Start a transport connection without waiting for the outcome.
    fn open_attempt(&mut self) -> TestClient {
        let addr: SocketAddr = format!("10.0.0.{}:6000", self.next_client)
            .parse()
            .expect("addr");
        self.next_client += 1;
        self.open_attempt_from(addr)
    }

    fn open_attempt_from(&mut self, addr: SocketAddr) -> TestClient {
        let driver = Driver::new_client(Some(self.fingerprint), Budget::client()).expect("client");
        let node = self.net.add_node(driver, addr);
        self.net.set_link(
            self.server,
            node,
            LinkSpec::clean(Duration::from_millis(10)),
        );
        let now = self.net.now();
        let peer = self
            .net
            .driver(node)
            .connect(now, SERVER_ADDR.parse().expect("addr"))
            .expect("connect");
        TestClient {
            node,
            peer,
            id: 0,
            inbox: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Connect, say Hello, and wait for `Init` (panics on refusal).
    fn connect(&mut self, name: &str, password: Option<&str>) -> TestClient {
        let mut client = self.open();
        client.hello(self, name, password);
        let id = client.wait_for(self, &mut [], |m| match m {
            ServerMessage::Init { client_id, .. } => Some(*client_id),
            _ => None,
        });
        client.id = id;
        client
    }

    /// Connect from a chosen source address and say Hello, returning the
    /// server's verdict.
    fn try_connect_from(
        &mut self,
        addr: SocketAddr,
        name: &str,
        password: Option<&str>,
    ) -> Result<TestClient, RefusedReason> {
        let mut client = self.open_attempt_from(addr);
        let deadline = self.net.now() + WAIT;
        while !self.net.driver(client.node).is_connected(client.peer) {
            assert!(self.net.now() < deadline, "handshake timed out");
            self.pump(&mut [&mut client]);
        }
        self.admission(&mut client, name, password).map(|()| client)
    }

    fn admission(
        &mut self,
        client: &mut TestClient,
        name: &str,
        password: Option<&str>,
    ) -> Result<(), RefusedReason> {
        client.hello(self, name, password);
        let deadline = self.net.now() + WAIT;
        loop {
            self.pump(&mut [client]);
            for m in &client.inbox {
                match m {
                    ServerMessage::Welcome { client_id, .. } => {
                        client.id = *client_id;
                        return Ok(());
                    }
                    ServerMessage::Refused { reason } => return Err(*reason),
                    _ => {}
                }
            }
            assert!(self.net.now() < deadline, "no admission verdict");
        }
    }
}

impl TestClient {
    fn hello(&mut self, world: &mut World, name: &str, password: Option<&str>) {
        self.send(
            world,
            &ClientMessage::Hello {
                username: name.to_string(),
                auth: Auth::Shared {
                    password: password.map(str::to_string),
                },
                client_key: None,
                features: features::CLIENT_1_0,
                client_version: protocol::CLIENT_VERSION.to_string(),
            },
        );
    }

    fn send(&mut self, world: &mut World, msg: &ClientMessage) {
        let bytes = protocol::encode_client_message(msg).expect("encode");
        let now = world.net.now();
        world
            .net
            .driver(self.node)
            .send(now, self.peer, msg.lane(), &bytes)
            .expect("send");
    }

    /// Send an image the way the real client does — chunked on one
    /// Media stream.
    fn send_image(&mut self, world: &mut World, upload_id: u32, blob: &[u8]) {
        let count = u32::try_from(blob.len().div_ceil(protocol::IMAGE_CHUNK_BYTES)).unwrap();
        let frames: Vec<protocol::Bytes> = blob
            .chunks(protocol::IMAGE_CHUNK_BYTES)
            .enumerate()
            .map(|(i, chunk)| {
                let msg = ClientMessage::ChatImageChunk {
                    upload_id,
                    index: u32::try_from(i).unwrap(),
                    count,
                    data: chunk.to_vec(),
                };
                protocol::Bytes::from(protocol::encode_client_message(&msg).expect("encode"))
            })
            .collect();
        let now = world.net.now();
        world
            .net
            .driver(self.node)
            .send_media(now, self.peer, frames)
            .expect("send_media");
    }

    /// Completed relayed images seen so far: fold the chunk stream in
    /// the inbox into per-(sender, upload) tallies.
    fn images_completed(&self) -> usize {
        let mut got: HashMap<(u64, u32), (u32, u32)> = HashMap::new();
        for msg in &self.inbox {
            if let ServerMessage::ChatImageChunk {
                client_id,
                upload_id,
                count,
                ..
            } = msg
            {
                let e = got.entry((*client_id, *upload_id)).or_insert((0, *count));
                e.0 += 1;
            }
        }
        got.values()
            .filter(|(received, count)| received >= count)
            .count()
    }

    /// Pump until `pred` finds a match in this client's inbox
    /// (returned), or panic after 5 s of simulated time.
    fn wait_for<T>(
        &mut self,
        world: &mut World,
        others: &mut [&mut TestClient],
        mut pred: impl FnMut(&ServerMessage) -> Option<T>,
    ) -> T {
        let deadline = world.net.now() + WAIT;
        loop {
            if let Some(found) = self.inbox.iter().find_map(&mut pred) {
                return found;
            }
            assert!(
                world.net.now() < deadline,
                "timed out waiting for message; inbox: {:?}",
                self.inbox
            );
            let mut all: Vec<&mut TestClient> = vec![self];
            all.extend(others.iter_mut().map(|c| &mut **c));
            world.pump(&mut all);
        }
    }

    fn join(&mut self, world: &mut World, channel_id: u16, password: Option<&str>) {
        self.send(
            world,
            &ClientMessage::JoinChannel {
                channel_id,
                password: password.map(str::to_string),
            },
        );
    }

    fn disconnected(&self) -> Option<&DisconnectReason> {
        self.events.iter().find_map(|e| match e {
            Event::Disconnected { reason, .. } => Some(reason),
            _ => None,
        })
    }
}

fn png_blob(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend(std::iter::repeat_n(seed, len.saturating_sub(v.len())));
    v
}

// ── Admission ───────────────────────────────────────────────────────────

#[test]
fn open_mode_admits_anyone_and_assigns_ids() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let a = w.connect("alice", None);
    let b = w.connect("bob", None);
    assert_ne!(a.id, 0);
    assert_ne!(a.id, b.id);
    let welcome = a
        .inbox
        .iter()
        .find(|m| matches!(m, ServerMessage::Welcome { .. }))
        .expect("welcome");
    assert!(matches!(
        welcome,
        ServerMessage::Welcome { client_id, user_id: None, features }
            if *client_id == a.id && *features == features::SERVER_1_0
    ));
}

#[test]
fn secure_mode_checks_the_password_and_rate_limits_failures() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.authentication.secure = true;
    config.authentication.password = Some("hunter2".to_string());
    let mut w = World::new(config);

    // Mallory keeps one address and rotates ports, as a real attacker
    // (or a NAT) would.
    let mut mallory_port = 7000u16;
    let mut mallory = |w: &mut World, password: Option<&str>| {
        let addr: SocketAddr = format!("10.0.0.99:{mallory_port}").parse().expect("addr");
        mallory_port += 1;
        w.try_connect_from(addr, "mallory", password).err()
    };

    // Wrong and missing passwords are refused.
    assert_eq!(
        mallory(&mut w, Some("wrong")),
        Some(RefusedReason::WrongPassword)
    );
    assert_eq!(mallory(&mut w, None), Some(RefusedReason::WrongPassword));

    // Right password from elsewhere: in.
    let alice = w.connect("alice", Some("hunter2"));
    assert!(alice
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Init { .. })));

    // Five failures from one address within a minute, then even the
    // right password is refused as rate-limited.
    for _ in 0..3 {
        assert_eq!(
            mallory(&mut w, Some("nope")),
            Some(RefusedReason::WrongPassword)
        );
    }
    assert_eq!(
        mallory(&mut w, Some("hunter2")),
        Some(RefusedReason::RateLimited)
    );
    // Rate-limited refusals do not extend the ban; after the window it
    // lifts.
    w.settle(&mut [], Duration::from_secs(61));
    assert_eq!(mallory(&mut w, Some("hunter2")), None);
}

#[test]
fn refused_connections_are_closed_after_the_verdict() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.authentication.secure = true;
    config.authentication.password = Some("pw".to_string());
    let mut w = World::new(config);
    let mut c = w.open();
    c.hello(&mut w, "x", Some("wrong"));
    let deadline = w.net.now() + WAIT;
    while c.disconnected().is_none() {
        assert!(w.net.now() < deadline, "never closed");
        w.pump(&mut [&mut c]);
    }
    assert!(matches!(
        c.disconnected(),
        Some(DisconnectReason::ApplicationClosed { .. })
    ));
    assert!(c
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Refused { .. })));
    assert!(!c
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Init { .. })));
}

#[test]
fn server_full_is_refused_at_hello() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.network.max_clients = 1;
    let mut w = World::new(config);
    let _a = w.connect("alice", None);
    // With one seat taken the transport already refuses the second
    // handshake; the app-level check is the backstop.
    let mut second = w.open_attempt();
    let deadline = w.net.now() + WAIT;
    while second.disconnected().is_none() && !w.net.driver(second.node).is_connected(second.peer) {
        assert!(w.net.now() < deadline, "no verdict");
        w.pump(&mut [&mut second]);
    }
    assert!(
        second.disconnected().is_some(),
        "second connection must be refused"
    );
}

#[test]
fn a_silent_connection_is_closed_without_hello() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut c = w.open();
    w.settle(&mut [&mut c], Duration::from_secs(12));
    assert!(
        c.disconnected().is_some(),
        "a connection that never says Hello must be closed"
    );
    assert!(c.inbox.is_empty());
}

#[test]
fn a_non_hello_first_message_is_refused() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut c = w.open();
    c.send(&mut w, &ClientMessage::Chat("hi".to_string()));
    let reason = c.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::Refused { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, RefusedReason::BadHello);
}

// ── Channels ────────────────────────────────────────────────────────────

#[test]
fn init_carries_channels_and_default_membership() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Gaming".to_string(),
            default: false,
            password: Some("hunter2".to_string()),
            user_limit: Some(8),
        },
    ]));
    let a = w.connect("alice", None);

    let (channels, memberships, my_id) = a
        .inbox
        .iter()
        .find_map(|m| match m {
            ServerMessage::Init {
                channels,
                memberships,
                client_id,
                ..
            } => Some((channels.clone(), memberships.clone(), *client_id)),
            _ => None,
        })
        .expect("init");

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
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Sekrit".to_string(),
            default: false,
            password: Some("hunter2".to_string()),
            user_limit: None,
        },
    ]));
    let mut a = w.connect("alice", None);

    // Wrong password → denied.
    a.join(&mut w, 1, Some("wrong"));
    a.wait_for(&mut w, &mut [], |m| {
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
    a.join(&mut w, 1, Some("hunter2"));
    let moved_to = a.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::UserMovedChannel { channel_id, .. } => Some(*channel_id),
        _ => None,
    });
    assert_eq!(moved_to, 1);

    // Bogus channel id → NoSuchChannel.
    a.join(&mut w, 99, None);
    a.wait_for(&mut w, &mut [], |m| {
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
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Duo".to_string(),
            default: false,
            password: None,
            user_limit: Some(1),
        },
    ]));
    let mut a = w.connect("alice", None);
    let mut b = w.connect("bob", None);

    a.join(&mut w, 1, None);
    a.wait_for(&mut w, &mut [&mut b], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // Channel full for the second client.
    b.join(&mut w, 1, None);
    b.wait_for(&mut w, &mut [&mut a], |m| {
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
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]));
    let mut sharer = w.connect("sharer", None);
    let mut viewer = w.connect("viewer", None);
    let sharer_id = sharer.id;

    // Sharer moves to Movies and starts a channel-scoped share there.
    sharer.join(&mut w, 1, None);
    sharer.wait_for(&mut w, &mut [&mut viewer], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });
    sharer.send(
        &mut w,
        &ClientMessage::ScreenShareStart {
            width: 1280,
            height: 720,
            scope: ShareScope::Channel,
        },
    );
    // The sharer's own channel hears the announcement…
    sharer.wait_for(&mut w, &mut [&mut viewer], |m| {
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
    viewer.send(
        &mut w,
        &ClientMessage::SubscribeScreen {
            target_id: sharer_id,
        },
    );
    viewer.wait_for(&mut w, &mut [&mut sharer], |m| {
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
    // subscribing now succeeds.
    viewer.join(&mut w, 1, None);
    viewer.wait_for(&mut w, &mut [&mut sharer], |m| {
        matches!(
            m,
            ServerMessage::ScreenShareStarted {
                client_id,
                scope: ShareScope::Channel,
                ..
            } if *client_id == sharer_id
        )
        .then_some(())
    });
    viewer.send(
        &mut w,
        &ClientMessage::SubscribeScreen {
            target_id: sharer_id,
        },
    );

    // Sharer moves back to the Lobby → the viewer (left behind in
    // Movies) is evicted: their view of the stream ends.
    sharer.join(&mut w, 0, None);
    viewer.wait_for(&mut w, &mut [&mut sharer], |m| {
        matches!(m, ServerMessage::ScreenShareStopped { client_id } if *client_id == sharer_id)
            .then_some(())
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
    // Secure mode needs a password; a stale 0.5 key is ignored.
    let config: Config = serde_yaml::from_str(
        "authentication:\n  secure: true\nnetwork:\n  bytes_per_tick: 4194304\n",
    )
    .expect("yaml");
    assert!(config.validate_authentication().is_err());
    let config: Config =
        serde_yaml::from_str("authentication:\n  secure: true\n  password: pw\n").expect("yaml");
    assert!(config.validate_authentication().is_ok());
}

#[test]
fn hot_reload_moves_renamed_channel_members_to_default() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]));
    let mut a = w.connect("alice", None);
    a.join(&mut w, 1, None);
    a.wait_for(&mut w, &mut [], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // "Movies" becomes "Cinema": same slot, new name ⇒ new channel ⇒
    // members go to the default.
    w.server()
        .apply_channel_config(vec![
            open_channel("Lobby", true),
            open_channel("Cinema", false),
        ])
        .expect("valid list");

    let (channels, memberships) = a.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::ChannelListUpdated {
            channels,
            memberships,
        } => Some((channels.clone(), memberships.clone())),
        _ => None,
    });
    assert_eq!(channels[1].name, "Cinema");
    assert_eq!(memberships.get(&a.id), Some(&0), "renamed ⇒ back to Lobby");
}

#[test]
fn hot_reload_password_change_keeps_members_but_gates_new_joins() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Duo", false),
    ]));
    let mut a = w.connect("alice", None);
    let mut b = w.connect("bob", None);
    a.join(&mut w, 1, None);
    a.wait_for(&mut w, &mut [&mut b], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // Duo gains a password. Alice (inside) stays; Bob now needs it.
    w.server()
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

    let memberships = b.wait_for(&mut w, &mut [&mut a], |m| match m {
        ServerMessage::ChannelListUpdated { memberships, .. } => Some(memberships.clone()),
        _ => None,
    });
    assert_eq!(
        memberships.get(&a.id),
        Some(&1),
        "existing member never kicked"
    );

    b.join(&mut w, 1, None);
    b.wait_for(&mut w, &mut [&mut a], |m| {
        matches!(
            m,
            ServerMessage::JoinChannelDenied {
                channel_id: 1,
                reason: ChannelDenyReason::WrongPassword,
            }
        )
        .then_some(())
    });
    b.join(&mut w, 1, Some("sesame"));
    let bob = b.id;
    b.wait_for(&mut w, &mut [&mut a], |m| {
        matches!(
            m,
            ServerMessage::UserMovedChannel {
                client_id,
                channel_id: 1
            } if *client_id == bob
        )
        .then_some(())
    });
}

#[test]
fn hot_reload_rederives_channel_scoped_share_visibility() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]));
    let mut sharer = w.connect("sharer", None);
    let mut viewer = w.connect("viewer", None);
    let mut lobbyist = w.connect("lobbyist", None);
    let sharer_id = sharer.id;
    let viewer_id = viewer.id;

    // Sharer + viewer set up a channel-scoped stream in Movies.
    sharer.join(&mut w, 1, None);
    viewer.join(&mut w, 1, None);
    viewer.wait_for(&mut w, &mut [&mut sharer, &mut lobbyist], |m| {
        matches!(
            m,
            ServerMessage::UserMovedChannel {
                client_id,
                channel_id: 1
            } if *client_id == viewer_id
        )
        .then_some(())
    });
    sharer.send(
        &mut w,
        &ClientMessage::ScreenShareStart {
            width: 1280,
            height: 720,
            scope: ShareScope::Channel,
        },
    );
    viewer.wait_for(&mut w, &mut [&mut sharer, &mut lobbyist], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { client_id, .. } if *client_id == sharer_id)
            .then_some(())
    });
    viewer.send(
        &mut w,
        &ClientMessage::SubscribeScreen {
            target_id: sharer_id,
        },
    );
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
    w.server()
        .apply_channel_config(vec![open_channel("Lobby", true)])
        .expect("valid list");

    lobbyist.wait_for(&mut w, &mut [&mut sharer, &mut viewer], |m| {
        matches!(
            m,
            ServerMessage::ScreenShareStarted {
                client_id,
                scope: ShareScope::Channel,
                ..
            } if *client_id == sharer_id
        )
        .then_some(())
    });
    assert!(
        !viewer.inbox.iter().any(|m| matches!(
            m,
            ServerMessage::ScreenShareStopped { client_id } if *client_id == sharer_id
        )),
        "viewer moved together with the sharer — must not be evicted"
    );
}

#[test]
fn hot_reload_rejects_invalid_lists_and_keeps_state() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]));
    let mut a = w.connect("alice", None);
    a.join(&mut w, 1, None);
    a.wait_for(&mut w, &mut [], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // No default / duplicate names: both rejected, nothing broadcast.
    assert!(w
        .server()
        .apply_channel_config(vec![open_channel("A", false)])
        .is_err());
    assert!(w
        .server()
        .apply_channel_config(vec![open_channel("A", true), open_channel("A", false)])
        .is_err());

    // Membership untouched: joining Movies again is a no-op (no deny,
    // no move), proving the live list survived the bad edits.
    a.inbox.clear();
    a.join(&mut w, 1, None);
    a.send(&mut w, &ClientMessage::Chat("ping".to_string()));
    let (id, ts) = a.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::Chat(c) if c.text == "ping" => Some((c.id, c.ts)),
        _ => None,
    });
    assert!(id > 0, "chat messages carry a server id");
    assert!(ts > 0, "chat messages carry a timestamp");
    assert!(
        !a.inbox.iter().any(|m| matches!(
            m,
            ServerMessage::JoinChannelDenied { .. } | ServerMessage::UserMovedChannel { .. }
        )),
        "state changed after rejected reloads"
    );
}

// ── Images ──────────────────────────────────────────────────────────────

#[test]
fn per_client_image_byte_budget_brakes_one_spammer() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_image_kb_per_minute = 100; // 100 KiB/min per client
    config.limits.max_image_kb_per_minute_global = 100_000; // loose
    let mut w = World::new(config);
    let mut a = w.connect("alice", None);
    let mut b = w.connect("bob", None);

    // Three ~40 KiB images from alice: 40+40 fits the 100 KiB window,
    // the third (120 KiB total) must be dropped. Bob's own budget is
    // untouched — his image still relays.
    for seed in 0..3u32 {
        a.send_image(
            &mut w,
            seed,
            &png_blob(u8::try_from(seed).unwrap(), 40 * 1024),
        );
    }
    b.send_image(&mut w, 0, &png_blob(9, 40 * 1024));

    w.settle(&mut [&mut a, &mut b], Duration::from_secs(3));
    let relayed = a.images_completed();
    assert_eq!(relayed, 3, "2 of alice's 3 + bob's 1 must relay");
    // Relayed chunks carry the content hash and a message stamp.
    let hashed = a.inbox.iter().any(|m| {
        matches!(
            m,
            ServerMessage::ChatImageChunk { hash, id, ts, .. }
                if *hash != [0; 32] && *id > 0 && *ts > 0
        )
    });
    assert!(hashed);
}

#[test]
fn global_image_byte_budget_brakes_coordinated_bursts() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_image_kb_per_minute = 100_000; // loose
    config.limits.max_image_kb_per_minute_global = 100; // 100 KiB/min room-wide
    let mut w = World::new(config);
    let mut a = w.connect("alice", None);
    let mut b = w.connect("bob", None);

    // 40 KiB from each side, alternating: the room window admits two
    // (80 KiB) and must drop the third and fourth.
    for (i, who) in [0u8, 1, 0, 1].iter().enumerate() {
        let blob = png_blob(u8::try_from(i).unwrap(), 40 * 1024);
        let upload_id = u32::try_from(i).unwrap();
        match who {
            0 => a.send_image(&mut w, upload_id, &blob),
            _ => b.send_image(&mut w, upload_id, &blob),
        }
        w.settle(&mut [&mut a, &mut b], Duration::from_millis(300));
    }

    w.settle(&mut [&mut a, &mut b], Duration::from_secs(3));
    assert_eq!(
        a.images_completed(),
        2,
        "room budget must stop the burst at 2"
    );
}

#[test]
fn relay_paces_large_images_instead_of_bursting() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_image_kb_per_minute = 100_000;
    config.limits.max_image_kb_per_minute_global = 100_000;
    config.limits.media_relay_kb_per_sec = 1024; // 1 MiB/s per receiver
    let mut w = World::new(config);
    let mut a = w.connect("alice", None);
    let mut b = w.connect("bob", None);

    // A 1 MiB image at 1 MiB/s must be *spread* over roughly a second
    // per receiver — the whole point is that it no longer lands as one
    // burst that queues ahead of voice. (The client's own upload is
    // paced too, at 512 KiB/s, so the whole trip is ~3 s.)
    a.send_image(&mut w, 1, &png_blob(3, 1024 * 1024));
    let started = w.net.now();
    let deadline = started + Duration::from_secs(15);
    while b.images_completed() == 0 {
        w.pump(&mut [&mut a, &mut b]);
        assert!(w.net.now() < deadline, "image never completed");
    }
    let took = w.net.now() - started;
    assert!(
        took >= Duration::from_millis(1500),
        "1 MiB must be paced on both hops, not burst (took {took:?})"
    );
    assert!(
        took <= Duration::from_secs(10),
        "pacing should finish in a few seconds, not stall (took {took:?})"
    );
}

// ── Media relay ─────────────────────────────────────────────────────────

#[test]
fn voice_is_relayed_within_the_channel_and_reported_back() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Other", false),
    ]));
    let mut a = w.connect("alice", None);
    let mut b = w.connect("bob", None);
    let mut c = w.connect("carol", None);
    c.join(&mut w, 1, None);
    c.wait_for(&mut w, &mut [&mut a, &mut b], |m| {
        matches!(m, ServerMessage::UserMovedChannel { channel_id: 1, .. }).then_some(())
    });

    // 100 voice packets at 20 ms from alice.
    let mut got_b = 0u32;
    let mut got_c = 0u32;
    for seq in 0..100u32 {
        let pkt = protocol::pack_voice(seq, seq * 20, &[0xAB; 60]).expect("voice");
        let now = w.net.now();
        w.net
            .driver(a.node)
            .send(now, a.peer, Lane::Voice, &pkt)
            .expect("send voice");
        w.settle(&mut [&mut a, &mut b, &mut c], Duration::from_millis(20));
        while let Some(d) = w.net.driver(b.node).recv(b.peer, Lane::Voice) {
            let (sender, payload) = protocol::split_sender(&d).expect("envelope");
            assert_eq!(sender, a.id);
            assert!(protocol::parse_voice(payload).is_some());
            got_b += 1;
        }
        while w.net.driver(c.node).recv(c.peer, Lane::Voice).is_some() {
            got_c += 1;
        }
    }
    w.settle(&mut [&mut a, &mut b, &mut c], Duration::from_secs(3));
    while w.net.driver(b.node).recv(b.peer, Lane::Voice).is_some() {
        got_b += 1;
    }
    assert_eq!(got_b, 100, "same-channel listener gets every packet");
    assert_eq!(got_c, 0, "other channel hears nothing");

    // Alice got receiver reports covering her packets.
    let reports: Vec<(u32, u32)> = a
        .inbox
        .iter()
        .filter_map(|m| match m {
            ServerMessage::VoiceReceiverReport {
                received, expected, ..
            } => Some((*received, *expected)),
            _ => None,
        })
        .collect();
    assert!(!reports.is_empty(), "no receiver reports");
    let received: u32 = reports.iter().map(|(r, _)| r).sum();
    let expected: u32 = reports.iter().map(|(_, e)| e).sum();
    assert_eq!(received, 100);
    assert_eq!(expected, 100);
}

#[test]
fn video_groups_are_mirrored_to_subscribers_on_the_sharers_track() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut sharer = w.connect("sharer", None);
    let mut viewer = w.connect("viewer", None);
    let sharer_id = sharer.id;

    sharer.send(
        &mut w,
        &ClientMessage::ScreenShareStart {
            width: 640,
            height: 360,
            scope: ShareScope::Everyone,
        },
    );
    viewer.wait_for(&mut w, &mut [&mut sharer], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { client_id, .. } if *client_id == sharer_id)
            .then_some(())
    });
    viewer.send(
        &mut w,
        &ClientMessage::SubscribeScreen {
            target_id: sharer_id,
        },
    );
    // Subscribing kicks the sharer for a keyframe.
    sharer.wait_for(&mut w, &mut [&mut viewer], |m| {
        matches!(m, ServerMessage::KeyframeRequested { .. }).then_some(())
    });

    // One group: a keyframe of 3 chunks, then two delta frames.
    let now = w.net.now();
    let drv = w.net.driver(sharer.node);
    drv.video_new_group(now, sharer.peer, OWN_TRACK);
    let key = protocol::pack_video_frame(1, true, 640, 360, 0, &[1; 3000]).expect("frame");
    for chunk in &key {
        drv.send(now, sharer.peer, Lane::Video, chunk)
            .expect("chunk");
    }
    for (seq, fill) in [(2u32, 2u8), (3, 3)] {
        let delta = protocol::pack_video_frame(seq, false, 640, 360, seq * 33, &[fill; 500])
            .expect("frame");
        for chunk in &delta {
            drv.send(now, sharer.peer, Lane::Video, chunk)
                .expect("chunk");
        }
    }
    drv.video_finish_group(now, sharer.peer, OWN_TRACK);

    let mut items = Vec::new();
    let deadline = w.net.now() + WAIT;
    while !matches!(items.last(), Some(VideoItem::GroupEnd)) {
        assert!(w.net.now() < deadline, "group never completed: {items:?}");
        w.pump(&mut [&mut sharer, &mut viewer]);
        while let Some((track, item)) = w.net.driver(viewer.node).recv_video(viewer.peer) {
            assert_eq!(track, sharer_id, "relayed groups ride the sharer's track");
            items.push(item);
        }
    }
    assert_eq!(items[0], VideoItem::GroupStart);
    let chunks: Vec<&protocol::Bytes> = items
        .iter()
        .filter_map(|i| match i {
            VideoItem::Chunk(b) => Some(b),
            _ => None,
        })
        .collect();
    assert_eq!(chunks.len(), 5, "3 keyframe chunks + 2 delta chunks");
    for chunk in chunks {
        let (sender, payload) = protocol::split_sender(chunk).expect("envelope");
        assert_eq!(sender, sharer_id);
        assert!(protocol::parse_video_header(payload).is_some());
    }
}
