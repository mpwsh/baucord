//! End-to-end server tests on the in-process network: a real
//! `ChatServer` and real client `Driver`s, every message crossing the
//! simulated wire through the full QUIC stack, on a fake clock.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use protocol::{
    features,
    permissions::{self, RoleId, ROLE_ADMIN, ROLE_GUEST, ROLE_MEMBER},
    Auth, ChannelDenyReason, ClientId, ClientKeyProof, ClientMessage, DenyReason,
    InviteDenyReason, InviteInvalidReason, Lane, NeedInviteReason, RefusedReason, RemovedKind,
    ServerMessage, ShareScope, UserId,
};
use server::{
    clock::Clock,
    config::{ChannelSettings, Config},
    policy::{Restriction, RestrictionKind},
    server::ChatServer,
};
use transport::{
    identity::{CLIENT_AUTH_EXPORTER_BYTES, CLIENT_AUTH_EXPORTER_LABEL},
    sim::{LinkSpec, NodeId, SimNet},
    Budget, ClientKey, DisconnectReason, Driver, Event, Fingerprint, PeerId, ServerIdentity,
    VideoItem, OWN_TRACK,
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
    // Memory mode unless a test turns the store on at a temp path.
    config.persistence.enabled = false;
    config
}

/// A fresh scratch directory for tests that need a real database.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "baucord-e2e-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// `test_config` with the `SQLite` store on at `dir/baucord.db`.
fn persistent_config(channels: Vec<ChannelSettings>, dir: &std::path::Path) -> Config {
    let mut config = test_config(channels);
    config.persistence.enabled = true;
    config.persistence.path = dir.join("baucord.db").to_string_lossy().into_owned();
    config
}

/// Rows in `messages` of the database at `dir`, read directly.
fn stored_message_count(dir: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(dir.join("baucord.db")).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .expect("count")
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
    /// A standing guest code, minted at start, so tests that are not
    /// about admission just `connect`.
    guest_code: String,
}

struct TestClient {
    node: NodeId,
    /// The server, as this client's driver names it.
    peer: PeerId,
    /// The id the server assigned to this client.
    id: ClientId,
    /// This device's key: the identity the server knows it by.
    key: Arc<ClientKey>,
    inbox: Vec<ServerMessage>,
    events: Vec<Event>,
}

/// Enough for `unwrap_err` on an admission verdict.
impl std::fmt::Debug for TestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestClient")
            .field("id", &self.id)
            .field("key", &self.key)
            .field("inbox", &self.inbox.len())
            .finish_non_exhaustive()
    }
}

impl World {
    fn new(config: Config) -> Self {
        let _ = env_logger::builder().is_test(true).try_init();
        let identity = ServerIdentity::generate();
        let fingerprint = identity.fingerprint();
        let mut server = ChatServer::new(config, &identity).expect("server start");
        let (guest_code, _) = server
            .mint_invite(None, ROLE_GUEST, None, None)
            .expect("guest invite");
        let mut net = SimNet::new(0xBA0C);
        let server = net.add_node(server, SERVER_ADDR.parse().expect("addr"));
        World {
            net,
            server,
            fingerprint,
            next_client: 2,
            guest_code,
        }
    }

    /// Mint a code granting `role` (as the operator would from the
    /// CLI, or an admin from the client).
    fn invite(&mut self, role: RoleId) -> String {
        self.server()
            .mint_invite(None, role, None, None)
            .expect("invite")
            .0
    }

    /// Jump the server's wall clock forward without ticking through
    /// the interval (invite expiry, restriction `until`).
    fn advance_wall(&mut self, by: Duration) {
        let now = self.net.now();
        let wall = self.server().clock().wall_ms(now)
            + u64::try_from(by.as_millis()).expect("fits");
        self.server().set_clock(Clock::fixed(wall, now));
    }

    /// The bootstrap admin code from the startup log.
    fn bootstrap_code(&mut self) -> String {
        self.server()
            .bootstrap_invite()
            .expect("bootstrap invite")
            .to_string()
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
            key: Arc::new(ClientKey::generate()),
            inbox: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Connect a fresh device as a guest through the standing guest
    /// code and wait for `Init` (panics on refusal).
    fn connect(&mut self, name: &str) -> TestClient {
        let code = self.guest_code.clone();
        self.connect_as(name, Auth::Invite { code })
    }

    /// Connect a fresh device with `auth` and wait for `Init`.
    fn connect_as(&mut self, name: &str, auth: Auth) -> TestClient {
        let key = Arc::new(ClientKey::generate());
        self.connect_with_key(name, auth, key)
    }

    /// Connect a device whose key the server may already know.
    fn connect_with_key(&mut self, name: &str, auth: Auth, key: Arc<ClientKey>) -> TestClient {
        let mut client = self.open();
        client.key = key;
        client.hello(self, name, auth);
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
        auth: Auth,
        key: Option<Arc<ClientKey>>,
    ) -> Result<TestClient, RefusedReason> {
        let mut client = self.open_attempt_from(addr);
        if let Some(key) = key {
            client.key = key;
        }
        let deadline = self.net.now() + WAIT;
        while !self.net.driver(client.node).is_connected(client.peer) {
            assert!(self.net.now() < deadline, "handshake timed out");
            self.pump(&mut [&mut client]);
        }
        self.admission(&mut client, name, auth).map(|()| client)
    }

    /// Say Hello with a fresh device and return the verdict.
    fn try_connect(&mut self, name: &str, auth: Auth) -> Result<TestClient, RefusedReason> {
        let mut client = self.open();
        self.admission(&mut client, name, auth).map(|()| client)
    }

    /// Say Hello with a known device and return the verdict.
    fn try_connect_with_key(
        &mut self,
        name: &str,
        auth: Auth,
        key: Arc<ClientKey>,
    ) -> Result<TestClient, RefusedReason> {
        let mut client = self.open();
        client.key = key;
        self.admission(&mut client, name, auth).map(|()| client)
    }

    fn admission(
        &mut self,
        client: &mut TestClient,
        name: &str,
        auth: Auth,
    ) -> Result<(), RefusedReason> {
        client.hello(self, name, auth);
        let deadline = self.net.now() + WAIT;
        loop {
            self.pump(&mut [client]);
            for m in &client.inbox {
                match m {
                    ServerMessage::Welcome { client_id, .. } => {
                        client.id = *client_id;
                        return Ok(());
                    }
                    ServerMessage::Refused { reason } => return Err(reason.clone()),
                    _ => {}
                }
            }
            assert!(self.net.now() < deadline, "no admission verdict");
        }
    }
}

impl TestClient {
    /// `Hello` the way the 1.1 client sends it: the device key signs
    /// this connection's TLS exporter.
    fn hello(&mut self, world: &mut World, name: &str, auth: Auth) {
        let mut exported = [0u8; CLIENT_AUTH_EXPORTER_BYTES];
        world
            .net
            .driver(self.node)
            .export_keying_material(self.peer, CLIENT_AUTH_EXPORTER_LABEL, b"", &mut exported)
            .expect("exporter after handshake");
        let proof = ClientKeyProof {
            public_key: self.key.public_key(),
            signature: self.key.sign(&exported),
        };
        self.send(
            world,
            &ClientMessage::Hello {
                username: name.to_string(),
                auth,
                client_key: Some(proof),
                features: features::CLIENT_1_1,
                client_version: protocol::CLIENT_VERSION.to_string(),
            },
        );
    }

    /// `Hello` the way a 1.0 client sends it: the shared password, no key.
    fn hello_legacy(&mut self, world: &mut World, name: &str, password: Option<&str>) {
        self.send(
            world,
            &ClientMessage::Hello {
                username: name.to_string(),
                auth: Auth::Shared {
                    password: password.map(str::to_string),
                },
                client_key: None,
                features: features::CLIENT_1_0,
                client_version: "1.0.3".to_string(),
            },
        );
    }

    /// Close the connection the way the client's Disconnect button
    /// does, and let the server notice.
    fn leave(mut self, world: &mut World) {
        let now = world.net.now();
        world.net.driver(self.node).close(now, self.peer, "bye");
        world.settle(&mut [&mut self], Duration::from_secs(1));
    }

    /// The `Session` the server sent after `Welcome`.
    fn session(&self) -> Option<(UserId, u64, bool)> {
        self.inbox.iter().find_map(|m| match m {
            ServerMessage::Session {
                user_id,
                permissions,
                persistent,
            } => Some((*user_id, *permissions, *persistent)),
            _ => None,
        })
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
    let a = w.connect("alice");
    let b = w.connect("bob");
    assert_ne!(a.id, 0);
    assert_ne!(a.id, b.id);
    let welcome = a
        .inbox
        .iter()
        .find(|m| matches!(m, ServerMessage::Welcome { .. }))
        .expect("welcome");
    assert!(matches!(
        welcome,
        ServerMessage::Welcome { client_id, user_id: Some(user_id), features }
            if *client_id == a.id
                && *user_id == a.key.user_id()
                && *features == features::SERVER_1_1
    ));
    let (user_id, perms, persistent) = a.session().expect("Session after Welcome");
    assert_eq!(user_id, a.key.user_id());
    assert_eq!(perms, permissions::GUEST_PERMISSIONS);
    assert!(!persistent, "test servers run in memory mode");
}

// ── Admission (spec §3) ─────────────────────────────────────────────────

/// The first test of step B: with auto-reconnect, every blackout
/// produces "the user connects while their old session is still in
/// its idle window". That is a replacement, never `NameInUse`
/// against yourself.
#[test]
fn a_key_login_replaces_its_own_live_session() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");
    let bob_id = b.id;
    a.wait_for(&mut w, &mut [&mut b], |m| {
        matches!(m, ServerMessage::UserJoined { client_id, .. } if *client_id == bob_id)
            .then_some(())
    });
    let old_id = a.id;

    // A blackout: alice's old connection is never closed; her client
    // reconnects with the same key and the same name.
    let mut a2 = w.connect_with_key("alice", Auth::Key, Arc::clone(&a.key));
    assert_ne!(a2.id, old_id);
    assert_eq!(a2.session().map(|s| s.0), Some(a.key.user_id()));

    // The old session is told it was replaced, then closed.
    a.wait_for(&mut w, &mut [&mut a2, &mut b], |m| {
        matches!(
            m,
            ServerMessage::Removed {
                kind: RemovedKind::Replaced,
                ..
            }
        )
        .then_some(())
    });
    let deadline = w.net.now() + WAIT;
    while a.disconnected().is_none() {
        assert!(w.net.now() < deadline, "old session never closed");
        w.pump(&mut [&mut a, &mut a2, &mut b]);
    }

    // Bob saw the old alice leave and the new one arrive; the roster
    // holds one alice.
    assert!(b.inbox.iter().any(|m| matches!(
        m,
        ServerMessage::UserLeft { client_id } if *client_id == old_id
    )));
    assert!(b.inbox.iter().any(|m| matches!(
        m,
        ServerMessage::UserJoined { client_id, username, .. }
            if *client_id == a2.id && username == "alice"
    )));
    let user = a2.key.user_id();
    let sessions = w
        .server()
        .clients()
        .into_iter()
        .filter(|c| w.server().user_of(*c) == Some(user))
        .count();
    assert_eq!(sessions, 1);
}

#[test]
fn a_1_0_client_is_refused_with_bad_hello() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut c = w.open();
    c.hello_legacy(&mut w, "oldtimer", Some("hunter2"));
    let reason = c.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::Refused { reason } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(reason, RefusedReason::BadHello);
}

#[test]
fn an_unknown_key_needs_an_invite_and_that_does_not_count_as_a_failure() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    for _ in 0..8 {
        let err = w.try_connect("newcomer", Auth::Key).expect_err("unknown key");
        assert_eq!(
            err,
            RefusedReason::NeedInvite {
                why: NeedInviteReason::UnknownKey
            }
        );
    }
}

#[test]
fn an_invite_admits_once_and_the_key_logs_in_from_then_on() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let code = w.invite(ROLE_GUEST);
    let a = w.connect_as("alice", Auth::Invite { code: code.clone() });
    let (user_id, perms, _) = a.session().expect("session");
    assert_eq!(user_id, a.key.user_id(), "user id is the founding key's hash");
    assert_eq!(perms, permissions::GUEST_PERMISSIONS);
    assert_eq!(w.server().policy().invites[&code].uses, 1);
    assert_eq!(
        w.server().policy().users[&user_id].invited_with.as_deref(),
        Some(code.as_str())
    );

    let key = Arc::clone(&a.key);
    a.leave(&mut w);

    // Next visit: the key is the login, no code needed.
    let again = w.connect_with_key("alice", Auth::Key, Arc::clone(&key));
    assert_eq!(again.session().map(|s| s.0), Some(user_id));

    // A code sent anyway by a known key burns no use.
    again.leave(&mut w);
    let _third = w.connect_with_key("alice", Auth::Invite { code: code.clone() }, key);
    assert_eq!(w.server().policy().invites[&code].uses, 1);
}

#[test]
fn invalid_codes_say_which_and_count_toward_the_rate_limit() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let good = w.invite(ROLE_GUEST);

    // Mallory keeps one address and rotates ports, as a real attacker
    // (or a NAT) would.
    let mut mallory_port = 7000u16;
    let mut mallory = |w: &mut World, code: &str| {
        let addr: SocketAddr = format!("10.0.0.99:{mallory_port}").parse().expect("addr");
        mallory_port += 1;
        w.try_connect_from(
            addr,
            "mallory",
            Auth::Invite {
                code: code.to_string(),
            },
            None,
        )
        .err()
    };
    let invalid = |why| Some(RefusedReason::InviteInvalid { why });

    assert_eq!(
        mallory(&mut w, "ABCDEFGHJKMN"),
        invalid(InviteInvalidReason::Unknown)
    );
    assert_eq!(
        mallory(&mut w, "not a code"),
        invalid(InviteInvalidReason::Unknown)
    );

    // Alice, elsewhere, gets in fine.
    let alice = w.connect_as("alice", Auth::Invite { code: good.clone() });
    assert!(alice.session().is_some());

    // Five failures from one address within a minute, then even the
    // right code is refused as rate-limited.
    for _ in 0..3 {
        assert_eq!(
            mallory(&mut w, "ABCDEFGHJKMN"),
            invalid(InviteInvalidReason::Unknown)
        );
    }
    assert_eq!(mallory(&mut w, &good), Some(RefusedReason::RateLimited));
    // Rate-limited refusals do not extend the ban; after the window it
    // lifts.
    w.settle(&mut [], Duration::from_secs(61));
    assert_eq!(mallory(&mut w, &good), None);
}

#[test]
fn expired_exhausted_and_revoked_codes_are_told_apart() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let invalid = |why| RefusedReason::InviteInvalid { why };

    // Expiry rides on the tick's clock: run it forward.
    let (short, _) = w
        .server()
        .mint_invite(None, ROLE_GUEST, Some(60 * 60 * 1000), None)
        .expect("invite");
    w.advance_wall(Duration::from_hours(2));
    assert_eq!(
        w.try_connect("late", Auth::Invite { code: short })
            .unwrap_err(),
        invalid(InviteInvalidReason::Expired)
    );

    let (once, _) = w
        .server()
        .mint_invite(None, ROLE_GUEST, None, Some(1))
        .expect("invite");
    let _first = w.connect_as("first", Auth::Invite { code: once.clone() });
    assert_eq!(
        w.try_connect("second", Auth::Invite { code: once })
            .unwrap_err(),
        invalid(InviteInvalidReason::Exhausted)
    );

    let revoked = w.invite(ROLE_GUEST);
    assert!(w.server().revoke_invite(&revoked, None));
    assert_eq!(
        w.try_connect("nope", Auth::Invite { code: revoked })
            .unwrap_err(),
        invalid(InviteInvalidReason::Revoked)
    );

    // Codes are read out loud: case and separators do not matter.
    let code = w.invite(ROLE_GUEST);
    let spoken = format!(
        "{}-{} {}",
        &code[..4].to_ascii_lowercase(),
        &code[4..8],
        &code[8..].to_ascii_lowercase()
    );
    assert!(w.try_connect("spoken", Auth::Invite { code: spoken }).is_ok());
}

#[test]
fn the_bootstrap_code_makes_the_first_admin_exactly_once() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    assert!(!w.server().policy().has_admin());
    let code = w.bootstrap_code();
    let admin = w.connect_as("mpw", Auth::Invite { code: code.clone() });
    let (_, perms, _) = admin.session().expect("session");
    assert!(permissions::has(perms, permissions::ADMIN));
    assert!(w.server().policy().has_admin());
    assert!(w.server().bootstrap_invite().is_none(), "one shot");
    assert_eq!(
        w.try_connect("impostor", Auth::Invite { code })
            .unwrap_err(),
        RefusedReason::InviteInvalid {
            why: InviteInvalidReason::Exhausted
        }
    );
}

#[test]
fn online_duplicate_names_are_refused_case_insensitively() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let _alice = w.connect("alice");
    let code = w.guest_code.clone();
    assert_eq!(
        w.try_connect("ALICE", Auth::Invite { code: code.clone() })
            .unwrap_err(),
        RefusedReason::NameInUse
    );
    assert!(w.try_connect("alice2", Auth::Invite { code }).is_ok());
}

#[test]
fn a_guest_cannot_take_an_offline_members_name() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let member_code = w.invite(ROLE_MEMBER);
    let alice = w.connect_as(
        "Alice",
        Auth::Invite {
            code: member_code.clone(),
        },
    );
    let key = Arc::clone(&alice.key);
    alice.leave(&mut w);
    assert!(w.server().clients().is_empty(), "alice's session is gone");

    let guest_code = w.guest_code.clone();
    assert_eq!(
        w.try_connect("alice", Auth::Invite { code: guest_code })
            .unwrap_err(),
        RefusedReason::NameInUse
    );
    // Another member may; the rule is against guests impersonating.
    let other = w.connect_as("alice", Auth::Invite { code: member_code });
    other.leave(&mut w);
    // And Alice herself, of course — under a new name too: the name of
    // record follows the Hello.
    let back = w.connect_with_key("Alice again", Auth::Key, key);
    let user = back.session().expect("session").0;
    assert_eq!(w.server().policy().users[&user].name, "Alice again");
}

#[test]
fn a_ban_removes_the_session_and_refuses_the_key_until_it_lifts() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");
    let user_id = a.key.user_id();
    let until = w.server().clock().wall_ms(w.net.now()) + 60 * 60 * 1000;
    w.server().restrict(
        Restriction {
            id: 0,
            user_id: Some(user_id),
            key_hash: Some(user_id),
            ip: None,
            kind: RestrictionKind::Server,
            until: Some(until),
            reason: "spam".to_string(),
            by: None,
            created_at: 0,
        },
        "mpw",
    );
    a.wait_for(&mut w, &mut [&mut b], |m| match m {
        ServerMessage::Removed {
            kind: RemovedKind::Banned { until: got },
            reason,
            by,
        } if *got == Some(until) && reason == "spam" && by == "mpw" => Some(()),
        _ => None,
    });
    let old_id = a.id;
    b.wait_for(&mut w, &mut [&mut a], |m| {
        matches!(m, ServerMessage::UserLeft { client_id } if *client_id == old_id).then_some(())
    });

    // The key is refused with the reason, whatever name or code it tries.
    let key = Arc::clone(&a.key);
    let guest_code = w.guest_code.clone();
    assert_eq!(
        w.try_connect_with_key(
            "innocent",
            Auth::Invite { code: guest_code },
            Arc::clone(&key)
        )
        .unwrap_err(),
        RefusedReason::Banned {
            until: Some(until),
            reason: "spam".to_string()
        }
    );
    // After `until`, the key is a login again.
    w.advance_wall(Duration::from_hours(2));
    assert!(w.try_connect_with_key("alice", Auth::Key, key).is_ok());
}

#[test]
fn a_retired_key_needs_a_relink_even_with_a_code() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let a = w.connect("alice");
    let key = Arc::clone(&a.key);
    a.leave(&mut w);
    assert!(w.server().unlink_key(&key.user_id()));
    let need = RefusedReason::NeedInvite {
        why: NeedInviteReason::KeyRetired,
    };
    assert_eq!(
        w.try_connect_with_key("alice", Auth::Key, Arc::clone(&key))
            .unwrap_err(),
        need
    );
    let code = w.guest_code.clone();
    assert_eq!(
        w.try_connect_with_key("alice", Auth::Invite { code }, key)
            .unwrap_err(),
        need
    );
}

#[test]
fn members_mint_guest_codes_and_admins_choose() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let create = |grants_role, expires_secs| ClientMessage::CreateInvite {
        grants_role,
        expires_secs,
        max_uses: Some(1),
    };
    let answer = |c: &mut TestClient, w: &mut World| {
        c.wait_for(w, &mut [], |m| match m {
            ServerMessage::InviteCreated {
                code,
                grants_role,
                expires_at,
                max_uses,
            } => Some(Ok((code.clone(), *grants_role, *expires_at, *max_uses))),
            ServerMessage::InviteDenied { reason } => Some(Err(*reason)),
            _ => None,
        })
    };

    let mut guest = w.connect("guest");
    guest.send(&mut w, &create(None, None));
    assert_eq!(
        answer(&mut guest, &mut w),
        Err(InviteDenyReason::NoPermission)
    );

    let member_code = w.invite(ROLE_MEMBER);
    let mut member = w.connect_as("member", Auth::Invite { code: member_code });
    member.send(&mut w, &create(None, Some(7 * 24 * 3600)));
    let (code, role, expires_at, max_uses) = answer(&mut member, &mut w).expect("guest code");
    assert_eq!(role, ROLE_GUEST);
    assert_eq!(max_uses, Some(1));
    let now = w.server().clock().wall_ms(w.net.now());
    assert!(
        expires_at.expect("expiry") <= now + 24 * 60 * 60 * 1000 + 1000,
        "a member's code is clamped to the default expiry"
    );
    assert!(w.try_connect("via member", Auth::Invite { code }).is_ok());
    member.inbox.clear();
    member.send(&mut w, &create(Some(ROLE_MEMBER), None));
    assert_eq!(
        answer(&mut member, &mut w),
        Err(InviteDenyReason::NoPermission)
    );

    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("admin", Auth::Invite { code: boot });
    admin.send(&mut w, &create(Some(ROLE_MEMBER), Some(7 * 24 * 3600)));
    let (code, role, expires_at, _) = answer(&mut admin, &mut w).expect("member code");
    assert_eq!(role, ROLE_MEMBER);
    assert!(expires_at.expect("expiry") > now + 6 * 24 * 60 * 60 * 1000);
    let newbie = w.connect_as("newbie", Auth::Invite { code });
    assert_eq!(
        newbie.session().expect("session").1,
        permissions::MEMBER_PERMISSIONS
    );
    admin.inbox.clear();
    admin.send(&mut w, &create(Some(ROLE_ADMIN), None));
    assert_eq!(
        answer(&mut admin, &mut w),
        Err(InviteDenyReason::RoleNotAllowed)
    );
}

#[test]
fn a_refused_connection_is_closed_after_the_verdict() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut c = w.open();
    c.hello(
        &mut w,
        "x",
        Auth::Invite {
            code: "ABCDEFGHJKMN".to_string(),
        },
    );
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
fn policy_survives_a_restart_with_persistence_on() {
    let dir = temp_dir("policy");
    let key = Arc::new(ClientKey::generate());
    let user_id = key.user_id();
    {
        let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
        let code = w.invite(ROLE_MEMBER);
        let a = w.connect_with_key("alice", Auth::Invite { code }, Arc::clone(&key));
        assert!(a.session().expect("session").2, "persistent: true");
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.server().persistence().stats().queued > 0 {
            assert!(Instant::now() < deadline, "writes never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    assert_eq!(w.server().policy().users[&user_id].name, "alice");
    let again = w.connect_with_key("alice", Auth::Key, key);
    let (id, perms, persistent) = again.session().expect("session");
    assert_eq!(id, user_id);
    assert_eq!(perms, permissions::MEMBER_PERMISSIONS);
    assert!(persistent);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_code_minted_by_the_cli_is_picked_up_by_the_running_server() {
    let dir = temp_dir("cli");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let (code, _) = server::persistence::cli_create_invite(
        &dir.join("baucord.db"),
        ROLE_MEMBER,
        Some(60 * 60 * 1000),
        Some(1),
    )
    .expect("cli invite");
    // The persistence thread polls `data_version` every 2 s of real time.
    let deadline = Instant::now() + Duration::from_secs(6);
    while !w.server().policy().invites.contains_key(&code) {
        assert!(
            Instant::now() < deadline,
            "server never noticed the CLI's write"
        );
        std::thread::sleep(Duration::from_millis(50));
        w.pump(&mut []);
    }
    let joined = w.connect_as("cli guest", Auth::Invite { code });
    assert_eq!(
        joined.session().expect("session").1,
        permissions::MEMBER_PERMISSIONS
    );
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn server_full_is_refused_at_hello() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.network.max_clients = 1;
    let mut w = World::new(config);
    let _a = w.connect("alice");
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
        ServerMessage::Refused { reason } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(reason, RefusedReason::BadHello);
}

#[test]
fn shutdown_closes_every_session_with_the_restart_reason() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");
    let now = w.net.now();
    w.server().shutdown(now, "server restarting");
    let deadline = w.net.now() + WAIT;
    while a.disconnected().is_none() || b.disconnected().is_none() {
        assert!(w.net.now() < deadline, "clients never saw the close");
        w.pump(&mut [&mut a, &mut b]);
    }
    for c in [&a, &b] {
        assert!(
            matches!(
                c.disconnected(),
                Some(DisconnectReason::ApplicationClosed { reason, .. }) if reason == "server restarting"
            ),
            "{:?}",
            c.disconnected()
        );
    }
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
    let a = w.connect("alice");

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
    let mut a = w.connect("alice");

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
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");

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
    let mut sharer = w.connect("sharer");
    let mut viewer = w.connect("viewer");
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
    // A 1.0 `authentication:` section and a stale 0.5 key both still
    // parse (warned about and ignored at startup), and the section is
    // never written back.
    let config: Config = serde_yaml::from_str(
        "authentication:\n  secure: true\n  password: pw\nnetwork:\n  bytes_per_tick: 4194304\n",
    )
    .expect("yaml");
    assert!(config.authentication.secure);
    let written = serde_yaml::to_string(&config).expect("serialize");
    assert!(!written.contains("authentication"));
}

#[test]
fn hot_reload_moves_renamed_channel_members_to_default() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Movies", false),
    ]));
    let mut a = w.connect("alice");
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
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");
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
    let mut sharer = w.connect("sharer");
    let mut viewer = w.connect("viewer");
    let mut lobbyist = w.connect("lobbyist");
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
    let mut a = w.connect("alice");
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
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");

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
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");

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
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");

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
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");
    let mut c = w.connect("carol");
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
    let mut sharer = w.connect("sharer");
    let mut viewer = w.connect("viewer");
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

// ── Persistence (spec §1, §11: the first test of 1.1) ───────────────────

/// The persistence thread stalls — a slow disk — and nothing on the
/// media or delivery path notices: voice is relayed, every chat is
/// delivered, and the inserts that did not fit the queue are dropped
/// and counted rather than waited for. Once the disk is back, what was
/// queued lands.
#[test]
fn a_stalled_persistence_thread_never_touches_the_media_path() {
    let dir = temp_dir("stall");
    let mut config = persistent_config(vec![open_channel("Lobby", true)], &dir);
    // A short queue so the overflow shows with a handful of messages.
    config.persistence.write_queue = 8;
    let mut w = World::new(config);
    let mut a = w.connect("alice");
    let mut b = w.connect("bob");
    // Let the admission rows land first, so the queue holds chats only.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "admission rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let written_before = w.server().persistence().stats().written;

    // The disk stalls. The thread is real; give it a moment to park.
    let stalled = w.server().persistence().stall();
    std::thread::sleep(Duration::from_millis(50));

    // 20 chats: 8 fit the queue, 12 are dropped and counted.
    for i in 0..20 {
        a.send(&mut w, &ClientMessage::Chat(format!("m{i}")));
    }
    for i in 0..20 {
        let wanted = format!("m{i}");
        b.wait_for(&mut w, &mut [&mut a], |m| {
            matches!(m, ServerMessage::Chat(c) if c.text == wanted).then_some(())
        });
    }
    let stats = w.server().persistence().stats();
    assert_eq!(stats.dropped_writes, 12, "{stats}");
    assert_eq!(stats.written, written_before, "{stats}");

    // Voice keeps flowing while the disk is stuck.
    let mut got_b = 0u32;
    for seq in 0..50u32 {
        let pkt = protocol::pack_voice(seq, seq * 20, &[0xAB; 60]).expect("voice");
        let now = w.net.now();
        w.net
            .driver(a.node)
            .send(now, a.peer, Lane::Voice, &pkt)
            .expect("send voice");
        w.settle(&mut [&mut a, &mut b], Duration::from_millis(20));
        while w.net.driver(b.node).recv(b.peer, Lane::Voice).is_some() {
            got_b += 1;
        }
    }
    w.settle(&mut [&mut a, &mut b], Duration::from_secs(1));
    while w.net.driver(b.node).recv(b.peer, Lane::Voice).is_some() {
        got_b += 1;
    }
    assert_eq!(got_b, 50, "voice must not wait for the disk");

    // The disk comes back: the queued eight land.
    drop(stalled);
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().written < written_before + 8 {
        assert!(
            Instant::now() < deadline,
            "queued writes never landed: {}",
            w.server().persistence().stats()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(w);
    assert_eq!(stored_message_count(&dir), 8);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Message ids are rebased on the database's watermark at startup, so
/// history stays uniquely keyed across restarts.
#[test]
fn message_ids_continue_across_restarts_with_persistence_on() {
    let dir = temp_dir("restart");
    let key = Arc::new(ClientKey::generate());
    let last_id = {
        let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
        let code = w.guest_code.clone();
        let mut a = w.connect_with_key("alice", Auth::Invite { code }, Arc::clone(&key));
        let mut last = 0;
        for i in 0..3 {
            a.send(&mut w, &ClientMessage::Chat(format!("first run {i}")));
            last = a.wait_for(&mut w, &mut [], |m| match m {
                ServerMessage::Chat(c) if c.text == format!("first run {i}") => Some(c.id),
                _ => None,
            });
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while stored_message_count(&dir) < 3 {
            assert!(Instant::now() < deadline, "writes never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
        last
    };
    assert_eq!(stored_message_count(&dir), 3);

    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    // The key is known now: it is the login.
    let mut a = w.connect_with_key("alice", Auth::Key, key);
    a.send(&mut w, &ClientMessage::Chat("second run".to_string()));
    let id = a.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::Chat(c) if c.text == "second run" => Some(c.id),
        _ => None,
    });
    assert_eq!(id, last_id + 1, "ids continue after a restart");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// With persistence off, chat is live only: no database is created.
#[test]
fn persistence_off_writes_nothing_to_disk() {
    let dir = temp_dir("off");
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.persistence.path = dir.join("baucord.db").to_string_lossy().into_owned();
    let mut w = World::new(config);
    let mut a = w.connect("alice");
    a.send(&mut w, &ClientMessage::Chat("hello".to_string()));
    a.wait_for(&mut w, &mut [], |m| {
        matches!(m, ServerMessage::Chat(c) if c.text == "hello").then_some(())
    });
    let stats = w.server().persistence().stats();
    assert!(!stats.persistent);
    assert_eq!(stats.dropped_writes, 0);
    drop(w);
    assert!(!dir.join("baucord.db").exists(), "memory mode must not create a database");
    let _ = std::fs::remove_dir_all(&dir);
}
