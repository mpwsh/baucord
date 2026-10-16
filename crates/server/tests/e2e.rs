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
    AdminCommand, AdminError, AdminOutcome, AdminTarget, Auth, ChannelDenyReason, ChannelSpec,
    ChannelUiDenyReason, ClientId, ClientKeyProof, ClientMessage, ConvRef, DenyReason,
    DmFailReason, InviteDenyReason, InviteInvalidReason, Lane, NeedInviteReason, RefusedReason,
    RemovedKind, RestrictionKind, ServerMessage, ShareScope, UploadRejectReason, UserId,
};
use server::{
    clock::Clock,
    config::{ChannelSettings, Config},
    policy::Restriction,
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
    // Scenarios send a dozen lines in a few simulated milliseconds;
    // only `chat_is_rate_limited_per_session` wants the default bite.
    config.limits.chat.burst = 1000;
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
    /// Standing codes, minted at start, so tests that are not about
    /// admission just `connect` (a guest) or `connect_member`.
    guest_code: String,
    member_code: String,
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
    /// What `hello` announces; an older client is this minus a bit.
    features: u64,
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
        let (member_code, _) = server
            .mint_invite(None, ROLE_MEMBER, None, None)
            .expect("member invite");
        let mut net = SimNet::new(0xBA0C);
        let server = net.add_node(server, SERVER_ADDR.parse().expect("addr"));
        World {
            net,
            server,
            fingerprint,
            next_client: 2,
            guest_code,
            member_code,
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
        let wall =
            self.server().clock().wall_ms(now) + u64::try_from(by.as_millis()).expect("fits");
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
        // A store read out: let the real clock move too, or a loaded
        // machine runs the read's deadline out in simulated time
        // before the database thread is even scheduled.
        if self.server().persistence().reads_in_flight() > 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.net.step(TICK);
        self.run_server_and_collect(clients);
    }

    /// One pass the way the real main loop runs: asleep until the
    /// server's `next_wake` or the next datagram, whichever is first.
    /// The fixed tick of [`Self::pump`] hid a periodic job `next_wake`
    /// forgets: the server here would sleep past it (review 2026-10,
    /// P3-66).
    fn pump_on_wake(&mut self, clients: &mut [&mut TestClient]) {
        let now = self.net.now();
        let wake = self.server().next_wake();
        self.net
            .step_to_next_event(wake.saturating_duration_since(now));
        self.run_server_and_collect(clients);
    }

    fn run_server_and_collect(&mut self, clients: &mut [&mut TestClient]) {
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
            features: features::CLIENT_1_1,
        }
    }

    /// Connect a fresh device as a guest through the standing guest
    /// code and wait for `Init` (panics on refusal).
    fn connect(&mut self, name: &str) -> TestClient {
        let code = self.guest_code.clone();
        self.connect_as(name, Auth::Invite { code })
    }

    /// Connect a fresh device as a member: what the tests about media
    /// use, since sharing and uploading need `SHARE` / `UPLOAD`
    /// (spec §4).
    fn connect_member(&mut self, name: &str) -> TestClient {
        let code = self.member_code.clone();
        self.connect_as(name, Auth::Invite { code })
    }

    /// Connect a fresh device with `auth` and wait for `Init`.
    fn connect_as(&mut self, name: &str, auth: Auth) -> TestClient {
        let key = Arc::new(ClientKey::generate());
        self.connect_with_key(name, auth, key)
    }

    /// Connect a device whose key the server may already know, and
    /// step out of the lobby into the first room (spec §12, 1.1.5):
    /// every test here was written for a world where connecting made
    /// you visible in channel 0, which a join now does. Tests about
    /// the lobby itself use `connect_lobby`.
    fn connect_with_key(&mut self, name: &str, auth: Auth, key: Arc<ClientKey>) -> TestClient {
        let mut client = self.connect_lobby_with_key(name, auth, key);
        client.join(self, 0, None);
        let id = client.id;
        client.wait_for(self, &mut [], |m| match m {
            ServerMessage::UserMovedChannel {
                client_id,
                channel_id: 0,
            } if *client_id == id => Some(()),
            _ => None,
        });
        // The join's own echo would satisfy a later "wait for my move"
        // in a test; the inbox is left as a pre-lobby connect left it.
        client.inbox.retain(|m| {
            !matches!(m, ServerMessage::UserMovedChannel { client_id, channel_id: 0 }
                if *client_id == id)
        });
        client
    }

    /// Connect and stay in the lobby: announced to nobody until a
    /// `join`.
    fn connect_lobby(&mut self, name: &str) -> TestClient {
        let code = self.guest_code.clone();
        let key = Arc::new(ClientKey::generate());
        self.connect_lobby_with_key(name, Auth::Invite { code }, key)
    }

    fn connect_lobby_with_key(
        &mut self,
        name: &str,
        auth: Auth,
        key: Arc<ClientKey>,
    ) -> TestClient {
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
                features: self.features,
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

    /// Send an admin command tagged `request_id`.
    fn admin(&mut self, world: &mut World, request_id: u32, command: AdminCommand) {
        self.send(
            world,
            &ClientMessage::Admin {
                request_id,
                command,
            },
        );
    }

    /// Wait for the answer to `request_id`.
    fn admin_result(
        &mut self,
        world: &mut World,
        others: &mut [&mut TestClient],
        request_id: u32,
    ) -> Result<AdminOutcome, AdminError> {
        self.wait_for(world, others, |m| match m {
            ServerMessage::AdminResult {
                request_id: id,
                result,
            } if *id == request_id => Some(result.clone()),
            _ => None,
        })
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

    /// Announce and send a blob the way the client does (spec §7):
    /// `UploadBegin` on the signal lane, then the chunks on one Media
    /// stream when the server wants them. Returns the hash.
    fn upload_blob(
        &mut self,
        world: &mut World,
        upload_id: u32,
        mime: &str,
        bytes: &[u8],
    ) -> [u8; 32] {
        let sha256 = transport::sha256::digest(bytes);
        self.send(
            world,
            &ClientMessage::UploadBegin {
                upload_id,
                name: "avatar.png".into(),
                mime: mime.into(),
                size: u32::try_from(bytes.len()).unwrap(),
                sha256,
            },
        );
        let needed = self.wait_for(world, &mut [], |m| match m {
            ServerMessage::UploadAccept {
                upload_id: id,
                needed,
            } if *id == upload_id => Some(*needed),
            ServerMessage::UploadRejected {
                upload_id: id,
                reason,
            } if *id == upload_id => {
                panic!("upload rejected: {reason:?}")
            }
            _ => None,
        });
        if needed {
            self.send_blob_chunks(world, upload_id, bytes);
        }
        sha256
    }

    /// The chunks of an announced upload, on one paced Media stream.
    fn send_blob_chunks(&mut self, world: &mut World, upload_id: u32, bytes: &[u8]) {
        let count = u32::try_from(bytes.len().div_ceil(protocol::IMAGE_CHUNK_BYTES)).unwrap();
        let frames: Vec<protocol::Bytes> = bytes
            .chunks(protocol::IMAGE_CHUNK_BYTES)
            .enumerate()
            .map(|(i, chunk)| {
                let msg = ClientMessage::UploadChunk {
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

    /// A fetched blob reassembled from the inbox, once every chunk is in.
    fn blob_received(&self, blob_id: &[u8; 32]) -> Option<Vec<u8>> {
        let mut parts: Vec<Option<Vec<u8>>> = Vec::new();
        for msg in &self.inbox {
            if let ServerMessage::BlobChunk {
                blob_id: id,
                index,
                count,
                data,
            } = msg
                && id == blob_id
            {
                parts.resize(*count as usize, None);
                parts[*index as usize] = Some(data.clone());
            }
        }
        if parts.is_empty() || parts.iter().any(Option::is_none) {
            return None;
        }
        Some(parts.into_iter().flatten().flatten().collect())
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

/// A PNG as the relay reads one: the magic and an IHDR saying 16×16
/// (the relay checks a picture's size from its header), then `seed`
/// to `len` bytes.
fn png_blob(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&16u32.to_be_bytes());
    v.extend_from_slice(&16u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
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
        let err = w
            .try_connect("newcomer", Auth::Key)
            .expect_err("unknown key");
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
    assert_eq!(
        user_id,
        a.key.user_id(),
        "user id is the founding key's hash"
    );
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
        code[..4].to_ascii_lowercase(),
        &code[4..8],
        code[8..].to_ascii_lowercase()
    );
    assert!(w
        .try_connect("spoken", Auth::Invite { code: spoken })
        .is_ok());
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

// ── Moderation and roles (spec §4, §8) ──────────────────────────────────

#[test]
fn a_kick_forgets_the_device_so_a_new_invite_is_needed() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let member_code = w.invite(ROLE_MEMBER);
    let mut troll = w.connect_as("troll", Auth::Invite { code: member_code });
    let troll_user = troll.key.user_id();

    // By name: the admin types what they see in the roster.
    admin.admin(
        &mut w,
        1,
        AdminCommand::Kick {
            target: AdminTarget::Name("TROLL".to_string()),
            reason: "spam".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut troll], 1),
        Ok(AdminOutcome::Done)
    );
    troll.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::Removed {
            kind: RemovedKind::Kicked,
            reason,
            by,
        } if reason == "spam" && by == "mpw" => Some(()),
        _ => None,
    });
    let deadline = w.net.now() + WAIT;
    while troll.disconnected().is_none() {
        assert!(w.net.now() < deadline, "kicked session never closed");
        w.pump(&mut [&mut troll, &mut admin]);
    }

    // Nothing to lift: the device is simply unknown again, and the
    // roles are gone. A fresh code gets them back in — as whatever the
    // code grants.
    assert!(w.server().policy().restrictions.is_empty());
    assert!(w.server().policy().users[&troll_user].roles.is_empty());
    let key = Arc::clone(&troll.key);
    assert_eq!(
        w.try_connect_with_key("troll", Auth::Key, Arc::clone(&key))
            .unwrap_err(),
        RefusedReason::NeedInvite {
            why: NeedInviteReason::UnknownKey
        }
    );
    let code = w.guest_code.clone();
    let back = w.connect_with_key("troll", Auth::Invite { code }, key);
    let (user, perms, _) = back.session().expect("session");
    assert_eq!(user, troll_user, "same key, same user id");
    assert_eq!(perms, permissions::GUEST_PERMISSIONS);
}

#[test]
fn a_ban_blocks_the_key_under_any_name_and_lifts_on_time() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let alice_user = alice.key.user_id();

    admin.admin(
        &mut w,
        1,
        AdminCommand::Ban {
            target: AdminTarget::User(alice_user),
            duration_secs: Some(3600),
            reason: "cool off".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Ok(AdminOutcome::Done)
    );
    let until = alice.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::Removed {
            kind: RemovedKind::Banned { until },
            ..
        } => Some(*until),
        _ => None,
    });
    assert!(until.is_some());

    // Same key, a fresh code, another name: still out.
    let code = w.invite(ROLE_GUEST);
    let key = Arc::clone(&alice.key);
    assert!(matches!(
        w.try_connect_with_key("innocent", Auth::Invite { code }, Arc::clone(&key))
            .unwrap_err(),
        RefusedReason::Banned { .. }
    ));
    // Lifted by name while she is offline — the server resolves it.
    admin.admin(
        &mut w,
        2,
        AdminCommand::Unban {
            target: AdminTarget::Name("alice".to_string()),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [], 2),
        Ok(AdminOutcome::Done)
    );
    assert!(w
        .try_connect_with_key("alice", Auth::Key, Arc::clone(&key))
        .is_ok());

    // And a timed one runs out on its own.
    let mut alice = w.connect_with_key("alice", Auth::Key, key);
    admin.admin(
        &mut w,
        3,
        AdminCommand::Ban {
            target: AdminTarget::User(alice_user),
            duration_secs: Some(60),
            reason: "one minute".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 3),
        Ok(AdminOutcome::Done)
    );
    let key = Arc::clone(&alice.key);
    w.advance_wall(Duration::from_hours(2));
    // The sweep runs once a second of tick time.
    w.settle(&mut [&mut admin], Duration::from_secs(2));
    assert!(w.try_connect_with_key("alice", Auth::Key, key).is_ok());
}

#[test]
fn a_ban_on_a_shared_address_hits_only_the_named_user() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    // Two people behind one address (a household NAT, one machine).
    let shared: SocketAddr = "10.0.0.9:7001".parse().expect("addr");
    let code = w.guest_code.clone();
    let mut alice = w
        .try_connect_from(shared, "alice", Auth::Invite { code: code.clone() }, None)
        .expect("alice in");
    let shared2: SocketAddr = "10.0.0.9:7002".parse().expect("addr");
    let mut bob = w
        .try_connect_from(shared2, "bob", Auth::Invite { code }, None)
        .expect("bob in");
    w.settle(
        &mut [&mut admin, &mut alice, &mut bob],
        Duration::from_millis(200),
    );
    let alice_user = alice.key.user_id();

    admin.admin(
        &mut w,
        1,
        AdminCommand::Ban {
            target: AdminTarget::User(alice_user),
            duration_secs: None,
            reason: String::new(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 1),
        Ok(AdminOutcome::Done)
    );
    alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| {
        matches!(
            m,
            ServerMessage::Removed {
                kind: RemovedKind::Banned { .. },
                ..
            }
        )
        .then_some(())
    });
    w.settle(
        &mut [&mut admin, &mut alice, &mut bob],
        Duration::from_millis(500),
    );
    // Bob shares the address and stays; the admin stays; the row does
    // not carry the shared address at all.
    assert!(bob.disconnected().is_none(), "bob was caught by the ban");
    assert!(
        admin.disconnected().is_none(),
        "the admin banned themselves"
    );
    let bans = &w.server().policy().restrictions;
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].ip, None);
    assert_eq!(bans[0].user_id, Some(alice_user));
}

/// Review P0-1: a listing longer than one message. A session that
/// joins pages gets every row behind one result; an older one gets the
/// first message's worth, as it always did — and not an empty panel.
#[test]
fn a_long_admin_listing_arrives_in_pages_and_an_older_client_gets_the_first() {
    const CODES: usize = 1500;
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    for _ in 0..CODES {
        w.invite(ROLE_GUEST);
    }
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let key = admin.key.clone();

    admin.admin(&mut w, 1, AdminCommand::ListInvites);
    let Ok(AdminOutcome::Invites(last)) = admin.admin_result(&mut w, &mut [], 1) else {
        panic!("no list");
    };
    let mut codes: Vec<String> = Vec::new();
    let mut pages = 0;
    for m in &admin.inbox {
        if let ServerMessage::AdminRows {
            request_id: 1,
            rows: AdminOutcome::Invites(rows),
        } = m
        {
            pages += 1;
            codes.extend(rows.iter().map(|r| r.code.clone()));
        }
    }
    codes.extend(last.iter().map(|r| r.code.clone()));
    assert!(pages >= 2, "{pages} pages ahead of the result");
    assert!(codes.len() >= CODES + 2, "{} codes listed", codes.len());
    let distinct: std::collections::HashSet<&String> = codes.iter().collect();
    assert_eq!(distinct.len(), codes.len(), "no row twice");

    // The same admin on a client from before the bit.
    admin.leave(&mut w);
    let mut older = w.open();
    older.key = key;
    older.features = features::CLIENT_1_1 & !features::ADMIN_PAGES;
    w.admission(&mut older, "mpw", Auth::Key).expect("admitted");
    older.admin(&mut w, 2, AdminCommand::ListInvites);
    let Ok(AdminOutcome::Invites(first)) = older.admin_result(&mut w, &mut [], 2) else {
        panic!("no list");
    };
    assert!(
        !older
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::AdminRows { .. })),
        "a client that cannot join pages is sent none"
    );
    assert!(!first.is_empty() && first.len() < codes.len());
    let first: Vec<&String> = first.iter().map(|r| &r.code).collect();
    assert_eq!(first, codes.iter().take(first.len()).collect::<Vec<_>>());
}

#[test]
fn nobody_acts_on_themselves_or_on_an_equal_rank() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut guest = w.connect("guest");
    let admin_user = admin.key.user_id();
    let guest_user = guest.key.user_id();

    // A guest has no admin bits at all.
    guest.admin(
        &mut w,
        1,
        AdminCommand::Kick {
            target: AdminTarget::User(admin_user),
            reason: String::new(),
        },
    );
    assert_eq!(
        guest.admin_result(&mut w, &mut [&mut admin], 1),
        Err(AdminError::NoPermission)
    );

    // Not on yourself.
    admin.admin(
        &mut w,
        2,
        AdminCommand::Kick {
            target: AdminTarget::User(admin_user),
            reason: String::new(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [], 2),
        Err(AdminError::Yourself)
    );

    // Promote the guest to admin: now they are equal, and neither can
    // touch the other.
    admin.admin(
        &mut w,
        3,
        AdminCommand::SetRoles {
            target: AdminTarget::User(guest_user),
            roles: vec![ROLE_ADMIN],
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut guest], 3),
        Ok(AdminOutcome::Done)
    );
    let (_, perms, _) = guest.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::Session {
            user_id,
            permissions,
            persistent,
        } if permissions::has(*permissions, permissions::ADMIN) => {
            Some((*user_id, *permissions, *persistent))
        }
        _ => None,
    });
    assert_eq!(perms, permissions::ADMIN_PERMISSIONS);
    admin.admin(
        &mut w,
        4,
        AdminCommand::Kick {
            target: AdminTarget::User(guest_user),
            reason: String::new(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut guest], 4),
        Err(AdminError::Outranked)
    );

    // Two admins: one may step down; the last one may not.
    admin.admin(
        &mut w,
        5,
        AdminCommand::SetRoles {
            target: AdminTarget::User(admin_user),
            roles: vec![ROLE_MEMBER],
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut guest], 5),
        Ok(AdminOutcome::Done)
    );
    guest.admin(
        &mut w,
        6,
        AdminCommand::SetRoles {
            target: AdminTarget::User(guest_user),
            roles: vec![ROLE_MEMBER],
        },
    );
    assert_eq!(
        guest.admin_result(&mut w, &mut [&mut admin], 6),
        Err(AdminError::LastAdmin)
    );
}

#[test]
fn a_mute_drops_voice_at_the_relay_and_the_client_is_told_both_ways() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    let alice_user = alice.key.user_id();

    let voice_reaches_bob = |w: &mut World,
                             alice: &mut TestClient,
                             bob: &mut TestClient,
                             admin: &mut TestClient,
                             seq0: u32| {
        for seq in seq0..seq0 + 20 {
            let pkt = protocol::pack_voice(seq, seq * 20, &[0xAB; 60]).expect("voice");
            let now = w.net.now();
            w.net
                .driver(alice.node)
                .send(now, alice.peer, Lane::Voice, &pkt)
                .expect("send voice");
            w.settle(&mut [alice, bob, admin], Duration::from_millis(20));
        }
        w.settle(&mut [alice, bob, admin], Duration::from_millis(500));
        let mut got = 0;
        while w.net.driver(bob.node).recv(bob.peer, Lane::Voice).is_some() {
            got += 1;
        }
        got
    };
    assert_eq!(
        voice_reaches_bob(&mut w, &mut alice, &mut bob, &mut admin, 0),
        20
    );

    admin.admin(
        &mut w,
        1,
        AdminCommand::Restrict {
            target: AdminTarget::User(alice_user),
            kind: RestrictionKind::Voice,
            duration_secs: Some(30 * 60),
            reason: "hot mic".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 1),
        Ok(AdminOutcome::Done)
    );
    let until = alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| match m {
        ServerMessage::Restricted {
            kind: RestrictionKind::Voice,
            until,
            reason,
        } if reason == "hot mic" => Some(*until),
        _ => None,
    });
    assert!(until.is_some());
    assert_eq!(
        voice_reaches_bob(&mut w, &mut alice, &mut bob, &mut admin, 100),
        0
    );

    // Time runs out: the sweep lifts it and says so.
    w.advance_wall(Duration::from_mins(31));
    w.settle(
        &mut [&mut alice, &mut bob, &mut admin],
        Duration::from_secs(2),
    );
    alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| {
        matches!(
            m,
            ServerMessage::RestrictionLifted {
                kind: RestrictionKind::Voice
            }
        )
        .then_some(())
    });
    assert_eq!(
        voice_reaches_bob(&mut w, &mut alice, &mut bob, &mut admin, 200),
        20
    );
}

#[test]
fn a_text_restriction_refuses_chat_with_the_reason() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    // A member: she holds `UPLOAD`, so the mute alone decides the image.
    let mut alice = w.connect_member("alice");
    let alice_user = alice.key.user_id();

    admin.admin(
        &mut w,
        1,
        AdminCommand::Restrict {
            target: AdminTarget::User(alice_user),
            kind: RestrictionKind::GeneralText,
            duration_secs: None,
            reason: "be nice".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Ok(AdminOutcome::Done)
    );
    alice.inbox.clear();
    admin.inbox.clear();
    alice.send(&mut w, &ClientMessage::Chat("hello?".to_string()));
    alice.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::Restricted {
            kind: RestrictionKind::GeneralText,
            until: None,
            reason,
        } if reason == "be nice" => Some(()),
        _ => None,
    });
    w.settle(&mut [&mut alice, &mut admin], Duration::from_millis(500));
    assert!(
        !admin
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "hello?")),
        "a restricted user's chat must not be delivered"
    );
    // Pictures are chat too.
    admin.inbox.clear();
    alice.send_image(&mut w, 1, &png_blob(1, 4 * 1024));
    w.settle(&mut [&mut alice, &mut admin], Duration::from_secs(2));
    assert_eq!(
        admin.images_completed(),
        0,
        "a text-muted user's image must not be relayed"
    );

    // Lifted: the same image goes through, so the block above was the
    // mute and not something else.
    admin.admin(
        &mut w,
        2,
        AdminCommand::Unmute {
            target: AdminTarget::User(alice_user),
            kind: RestrictionKind::GeneralText,
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 2),
        Ok(AdminOutcome::Done)
    );
    alice.wait_for(&mut w, &mut [&mut admin], |m| {
        matches!(
            m,
            ServerMessage::RestrictionLifted {
                kind: RestrictionKind::GeneralText
            }
        )
        .then_some(())
    });
    admin.inbox.clear();
    alice.send_image(&mut w, 2, &png_blob(2, 4 * 1024));
    w.settle(&mut [&mut alice, &mut admin], Duration::from_secs(2));
    assert_eq!(
        admin.images_completed(),
        1,
        "an unmuted member's image relays"
    );
}

#[test]
fn roles_change_live_for_the_user_and_every_roster() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let alice_user = alice.key.user_id();

    // The roster carried alice as a guest.
    admin.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserDetails { user_id, roles, .. } if *user_id == alice_user => {
            Some(roles.clone())
        }
        _ => None,
    });
    admin.inbox.clear();
    alice.inbox.clear();

    admin.admin(
        &mut w,
        1,
        AdminCommand::SetRoles {
            target: AdminTarget::User(alice_user),
            roles: vec![ROLE_MEMBER],
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Ok(AdminOutcome::Done)
    );
    let perms = alice.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::Session { permissions, .. } => Some(*permissions),
        _ => None,
    });
    assert_eq!(perms, permissions::MEMBER_PERMISSIONS);
    let roles = admin.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserDetails { user_id, roles, .. } if *user_id == alice_user => {
            Some(roles.clone())
        }
        _ => None,
    });
    assert_eq!(roles, vec![ROLE_MEMBER]);
    assert_eq!(
        w.server().policy().rank_of(&alice_user),
        permissions::RANK_MEMBER
    );
}

#[test]
fn sharing_needs_the_permission_and_bypass_limits_ignores_the_cap() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_sharers = 1;
    let mut w = World::new(config);
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut guest = w.connect("guest");
    let member_code = w.invite(ROLE_MEMBER);
    let mut member = w.connect_as("member", Auth::Invite { code: member_code });

    let share = ClientMessage::ScreenShareStart {
        width: 1280,
        height: 720,
        scope: ShareScope::Everyone,
    };
    guest.send(&mut w, &share);
    let reason = guest.wait_for(&mut w, &mut [&mut admin, &mut member], |m| match m {
        ServerMessage::ShareDenied { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, DenyReason::NotPermitted);

    member.send(&mut w, &share);
    member.wait_for(&mut w, &mut [&mut admin, &mut guest], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { .. }).then_some(())
    });

    // The cap is one sharer; the admin has BYPASS_LIMITS.
    admin.send(&mut w, &share);
    let admin_id = admin.id;
    admin.wait_for(&mut w, &mut [&mut member, &mut guest], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { client_id, .. } if *client_id == admin_id)
            .then_some(())
    });
    assert_eq!(w.server().video_sharer_count(), 2);
}

#[test]
fn a_ban_written_by_the_cli_lands_on_a_live_session() {
    let dir = temp_dir("cli-ban");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let alice_user = alice.key.user_id();
    // Her row must be on disk before the CLI looks her up.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }

    let db = dir.join("baucord.db");
    let (found, _) = server::persistence::cli_find_user(&db, "ALICE").expect("by name");
    assert_eq!(found, alice_user);
    let id = server::persistence::cli_restrict(
        &db,
        Restriction {
            id: 0,
            user_id: Some(alice_user),
            key_hash: None,
            ip: None,
            kind: RestrictionKind::Server,
            until: None,
            reason: "from the box".to_string(),
            by: None,
            created_at: 0,
        },
    )
    .expect("cli restrict");
    assert_eq!(id, 1);

    // The running server polls data_version every 2 s of real time.
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut alice]);
        if alice.inbox.iter().any(|m| {
            matches!(
                m,
                ServerMessage::Removed {
                    kind: RemovedKind::Banned { until: None },
                    reason,
                    ..
                } if reason == "from the box"
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the CLI ban never reached the session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(server::persistence::cli_lift_restriction(&db, id).expect("lift"));
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mute from the CLI is told to the person, and so is its lift: the
/// client learns a mute only from `Restricted`, and the reload used to
/// apply it without a word.
#[test]
fn a_cli_mute_and_its_lift_are_told_to_the_session() {
    let dir = temp_dir("cli-mute");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let alice_user = alice.key.user_id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let db = dir.join("baucord.db");
    let id = server::persistence::cli_restrict(
        &db,
        Restriction {
            id: 0,
            user_id: Some(alice_user),
            key_hash: None,
            ip: None,
            kind: RestrictionKind::Voice,
            until: None,
            reason: "too loud".to_string(),
            by: None,
            created_at: 0,
        },
    )
    .expect("cli restrict");
    let wait = |alice: &mut TestClient,
                w: &mut World,
                what: &str,
                hit: &dyn Fn(&ServerMessage) -> bool| {
        // The running server polls data_version every 2 s of real time.
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            w.pump(&mut [&mut *alice]);
            if alice.inbox.iter().any(hit) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{what} never reached the session"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait(&mut alice, &mut w, "the mute", &|m| {
        matches!(m, ServerMessage::Restricted { kind: RestrictionKind::Voice, reason, .. }
            if reason == "too loud")
    });
    assert!(server::persistence::cli_lift_restriction(&db, id).expect("lift"));
    wait(&mut alice, &mut w, "the lift", &|m| {
        matches!(
            m,
            ServerMessage::RestrictionLifted {
                kind: RestrictionKind::Voice
            }
        )
    });
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_admin_moves_a_user_past_a_channel_password() {
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        ChannelSettings {
            name: "Backroom".to_string(),
            default: false,
            password: Some("hunter2".to_string()),
            user_limit: None,
        },
    ]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let alice_user = alice.key.user_id();

    admin.admin(
        &mut w,
        1,
        AdminCommand::MoveUser {
            target: AdminTarget::User(alice_user),
            channel_id: 1,
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Ok(AdminOutcome::Done)
    );
    let alice_id = alice.id;
    alice.wait_for(&mut w, &mut [&mut admin], |m| {
        matches!(
            m,
            ServerMessage::UserMovedChannel { client_id, channel_id: 1 } if *client_id == alice_id
        )
        .then_some(())
    });
    assert_eq!(w.server().channel_of(alice_id), Some(1));
}

// ── History, progress, channels from the UI (spec §5, §6, §9) ──────────

#[test]
fn history_replays_on_join_and_pages_back_through_the_store() {
    let dir = temp_dir("history");
    let mut config = persistent_config(vec![open_channel("Lobby", true)], &dir);
    config.persistence.history.replay = 5;
    let mut w = World::new(config);
    let mut alice = w.connect("alice");
    for i in 0..12 {
        alice.send(&mut w, &ClientMessage::Chat(format!("line {i}")));
        alice.wait_for(&mut w, &mut [], |m| match m {
            ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. }
                if c.text == format!("line {i}") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    // The newcomer gets the last five, oldest first, with the author's
    // name — and a `more` that leads further back.
    let mut bob = w.connect("bob");
    let (page, more) = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::History {
            conv: ConvRef::General,
            messages,
            more,
        } => Some((messages.clone(), *more)),
        _ => None,
    });
    assert_eq!(page.len(), 5);
    assert_eq!(page[0].text, "line 7");
    assert_eq!(page[4].text, "line 11");
    assert_eq!(page[0].name, "alice");
    assert!(page.iter().all(|m| m.user_id == alice.key.user_id()));
    assert!(more);

    // Paging back: the ring covers the rest of the session.
    bob.inbox.clear();
    bob.send(
        &mut w,
        &ClientMessage::FetchHistory {
            conv: ConvRef::General,
            before: Some(page[0].id),
            limit: 4,
        },
    );
    let older = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::History { messages, .. } => Some(messages.clone()),
        _ => None,
    });
    assert_eq!(
        older.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        ["line 3", "line 4", "line 5", "line 6"]
    );

    // After a restart the ring is primed from the database.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "writes never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(w);
    let mut config = persistent_config(vec![open_channel("Lobby", true)], &dir);
    config.persistence.history.replay = 3;
    let mut w = World::new(config);
    let mut carol = w.connect("carol");
    let (page, more) = carol.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::History { messages, more, .. } => Some((messages.clone(), *more)),
        _ => None,
    });
    assert_eq!(
        page.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        ["line 9", "line 10", "line 11"]
    );
    assert!(more);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stalled_store_answers_history_with_a_retry_hint() {
    let dir = temp_dir("history-stall");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let stalled = w.server().persistence().stall();
    std::thread::sleep(Duration::from_millis(50));
    // A page the ring cannot answer: before the oldest id there is.
    alice.send(
        &mut w,
        &ClientMessage::FetchHistory {
            conv: ConvRef::General,
            before: Some(1),
            limit: 10,
        },
    );
    let retry = alice.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::HistoryUnavailable { retry_secs, .. } => Some(*retry_secs),
        _ => None,
    });
    assert!(retry > 0);
    drop(stalled);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn authors_edit_and_delete_their_own_recent_messages() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    alice.send(&mut w, &ClientMessage::Chat("typo".to_string()));
    let id = alice.wait_for(&mut w, &mut [&mut bob, &mut admin], |m| match m {
        ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "typo" => {
            Some(c.id)
        }
        _ => None,
    });

    // Bob cannot edit alice's message; alice can.
    bob.send(
        &mut w,
        &ClientMessage::EditMessage {
            msg_id: id,
            text: "hijack".to_string(),
        },
    );
    alice.send(
        &mut w,
        &ClientMessage::EditMessage {
            msg_id: id,
            text: "fixed".to_string(),
        },
    );
    let text = bob.wait_for(&mut w, &mut [&mut alice, &mut admin], |m| match m {
        ServerMessage::MessageEdited { msg_id, text } if *msg_id == id => Some(text.clone()),
        _ => None,
    });
    assert_eq!(text, "fixed");
    assert!(!bob
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::MessageEdited { text, .. } if text == "hijack")));

    // A newcomer sees the edit and the flag.
    let mut carol = w.connect("carol");
    let page = carol.wait_for(
        &mut w,
        &mut [&mut alice, &mut bob, &mut admin],
        |m| match m {
            ServerMessage::History { messages, .. } => Some(messages.clone()),
            _ => None,
        },
    );
    assert_eq!(page[0].text, "fixed");
    assert!(page[0].edited);

    // An admin deletes it for everyone.
    admin.admin(&mut w, 1, AdminCommand::DeleteMessage { msg_id: id });
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob, &mut carol], 1),
        Ok(AdminOutcome::Done)
    );
    carol.wait_for(&mut w, &mut [&mut alice, &mut bob, &mut admin], |m| {
        matches!(m, ServerMessage::MessageDeleted { msg_id } if *msg_id == id).then_some(())
    });
    let mut dave = w.connect("dave");
    w.settle(&mut [&mut dave], Duration::from_millis(300));
    assert!(dave.inbox.iter().all(|m| match m {
        ServerMessage::History { messages, .. } => messages.iter().all(|x| x.id != id),
        _ => true,
    }));
}

#[test]
fn chat_is_rate_limited_per_session() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.chat.burst = 3;
    config.limits.chat.per_secs = 3;
    let mut w = World::new(config);
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    for i in 0..5 {
        alice.send(&mut w, &ClientMessage::Chat(format!("burst {i}")));
    }
    alice.wait_for(&mut w, &mut [&mut bob], |m| {
        matches!(m, ServerMessage::ChatRateLimited { .. }).then_some(())
    });
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));
    let delivered = bob
        .inbox
        .iter()
        .filter(|m| matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text.starts_with("burst")))
        .count();
    assert_eq!(delivered, 3, "three in the burst, the rest dropped");
    // Tokens come back with time.
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(3));
    alice.send(&mut w, &ClientMessage::Chat("later".to_string()));
    bob.wait_for(&mut w, &mut [&mut alice], |m| {
        matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "later").then_some(())
    });
}

#[test]
fn progress_accrues_by_the_minute_and_levels_up() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    let alice_user = alice.key.user_id();
    // Alice talks (with bob in the room) for two minutes: 6 xp/min.
    for _ in 0..2 {
        alice.send(&mut w, &ClientMessage::Speaking(true));
        w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(61));
    }
    let xp = w
        .server()
        .policy()
        .xp
        .get(&alice_user)
        .copied()
        .unwrap_or(0);
    assert_eq!(xp, 12);
    // Chat earns nothing; a third minute of talking crosses 13 xp.
    alice.send(&mut w, &ClientMessage::Chat("gg".to_string()));
    alice.send(&mut w, &ClientMessage::Speaking(true));
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(61));
    let (xp, level) = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::ProgressUpdate { xp, level } => Some((*xp, *level)),
        _ => None,
    });
    assert_eq!(level, 2);
    assert!(xp >= 13);
    bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserDetails { user_id, level, .. } if *user_id == alice_user => {
            (*level == 2).then_some(())
        }
        _ => None,
    });
}

#[test]
fn custom_roles_come_from_the_cli_and_reach_the_room() {
    use server::persistence::{
        cli_find_user, cli_role_create, cli_role_delete, cli_role_edit, cli_set_roles, RoleEdit,
    };
    let dir = temp_dir("custom-roles");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let alice_user = alice.key.user_id();

    // The table arrives on join: the built-ins, in rank order.
    let names = alice.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::RoleList { roles } => {
            Some(roles.iter().map(|r| r.name.clone()).collect::<Vec<_>>())
        }
        _ => None,
    });
    assert_eq!(names, ["guest", "member", "organizer", "admin"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let db = dir.join("baucord.db");

    // The rules: a strict name, never the admin bit, and built-ins stay
    // as the binary says. The rank follows the set: everything a
    // member has sits just above members.
    let streamer = permissions::MEMBER_PERMISSIONS | permissions::MANAGE_CHANNELS;
    assert!(cli_role_create(&db, "Streamer", streamer, "", 0).is_err());
    assert!(cli_role_create(&db, "member", streamer, "", 0).is_err());
    assert!(cli_role_create(&db, "gods", streamer | permissions::ADMIN, "", 0).is_err());
    assert!(
        cli_role_create(&db, "odd", streamer, "unicorn", 0).is_err(),
        "icon list"
    );
    assert!(cli_role_edit(&db, "member", &RoleEdit::default()).is_err());
    let id = cli_role_create(&db, "streamer", streamer, "star", 0x00E0_6C75).expect("create");
    assert!(id > ROLE_ADMIN);
    assert!(
        cli_role_create(&db, "streamer", streamer, "", 0).is_err(),
        "name taken"
    );
    let rank = permissions::derived_rank(streamer);
    assert!(rank > permissions::RANK_MEMBER && rank < permissions::RANK_ORGANIZER);

    // Grant it to alice by name; her session follows within the poll.
    let (found, _) = cli_find_user(&db, "alice").expect("alice on disk");
    assert_eq!(found, alice_user);
    cli_set_roles(&db, &alice_user, &[id]).expect("grant");
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut alice]);
        let followed = alice.inbox.iter().any(
            |m| matches!(m, ServerMessage::Session { permissions, .. } if *permissions == streamer),
        );
        let listed = alice.inbox.iter().any(|m| {
            matches!(m, ServerMessage::RoleList { roles }
                if roles.iter().any(|r| r.id == id && r.name == "streamer" && r.rank == rank
                    && r.icon == "star" && r.color == 0x00E0_6C75))
        });
        if followed && listed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the custom role never reached the session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(w.server().policy().users[&alice_user].roles.contains(&id));

    // Renamed in place, and the rank follows a bigger set; then
    // deleted, and she has nothing.
    let bigger = streamer | permissions::CREATE_CHANNELS | permissions::MANAGE_OWN_CHANNELS;
    let edit = RoleEdit {
        name: Some("caster".into()),
        permissions: Some(bigger),
        icon: Some("crown".into()),
        color: None,
    };
    assert_eq!(cli_role_edit(&db, "streamer", &edit).expect("edit"), id);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut alice]);
        let above_organizers = |r: &server::policy::RoleRecord| {
            r.rank > permissions::RANK_ORGANIZER && r.rank < permissions::RANK_ADMIN
        };
        if w.server()
            .policy()
            .roles
            .get(&id)
            .is_some_and(|r| r.name == "caster" && above_organizers(r))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the edit never reached the server"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, name) = cli_role_delete(&db, "caster").expect("delete");
    assert_eq!(name, "caster");
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut alice]);
        if w.server().policy().users[&alice_user].roles.is_empty()
            && !w.server().policy().roles.contains_key(&id)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the deletion never reached the server"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Upload `bytes` as a file named `name` and post it into `conv`;
/// returns the message id the server acked.
// A test helper that plays one whole upload; a struct for its eight
// knobs would be read in exactly one place.
#[expect(clippy::too_many_arguments)]
fn post_file(
    who: &mut TestClient,
    w: &mut World,
    peers: &mut [&mut TestClient],
    upload_id: u32,
    name: &str,
    mime: &str,
    bytes: &[u8],
    conv: ConvRef,
) -> Result<(u64, [u8; 32]), DmFailReason> {
    let sha256 = transport::sha256::digest(bytes);
    who.send(
        w,
        &ClientMessage::UploadBegin {
            upload_id,
            name: name.into(),
            mime: mime.into(),
            size: u32::try_from(bytes.len()).unwrap(),
            sha256,
        },
    );
    let needed = who.wait_for(w, peers, |m| match m {
        ServerMessage::UploadAccept {
            upload_id: id,
            needed,
        } if *id == upload_id => Some(*needed),
        ServerMessage::UploadRejected {
            upload_id: id,
            reason,
        } if *id == upload_id => panic!("upload rejected: {reason:?}"),
        _ => None,
    });
    if needed {
        who.send_blob_chunks(w, upload_id, bytes);
        who.wait_for(w, peers, |m| match m {
            ServerMessage::UploadDone { upload_id: id, .. } if *id == upload_id => Some(()),
            ServerMessage::UploadRejected {
                upload_id: id,
                reason,
            } if *id == upload_id => panic!("upload rejected: {reason:?}"),
            _ => None,
        });
    }
    let client_ref = upload_id + 1_000;
    who.send(
        w,
        &ClientMessage::SendFile {
            conv,
            blob_id: sha256,
            client_ref,
        },
    );
    who.wait_for(w, peers, |m| match m {
        ServerMessage::MessageAck {
            client_ref: r,
            msg_id,
        } if *r == client_ref => Some(Ok((*msg_id, sha256))),
        ServerMessage::MessageFailed {
            client_ref: r,
            reason,
        } if *r == client_ref => Some(Err(*reason)),
        _ => None,
    })
}

#[test]
fn files_are_posted_as_cards_and_replayed() {
    let dir = temp_dir("files");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_id = alice.id;
    let notes = b"just some notes, not an image".repeat(40);

    // A member posts a text file: everyone gets the card, the poster
    // an ack, and the blob is theirs to fetch.
    let (msg_id, blob) = post_file(
        &mut alice,
        &mut w,
        &mut [&mut bob],
        1,
        "../../notes.txt",
        "text/plain",
        &notes,
        ConvRef::General,
    )
    .expect("posted");
    let card = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::FileMessage {
            conv: ConvRef::General,
            msg,
            attachment,
            ..
        } if msg.id == msg_id => Some(attachment.clone()),
        _ => None,
    });
    assert_eq!(card.blob_id, blob);
    assert_eq!(card.name, "notes.txt", "sanitized: no path");
    assert_eq!(card.mime, "text/plain");
    assert_eq!(card.size, u32::try_from(notes.len()).unwrap());
    assert!(alice.inbox.iter().any(|m| matches!(m,
        ServerMessage::FileMessage { msg, .. } if msg.id == msg_id && msg.client_id == alice_id)));
    // The bytes live on disk only: let the write land before the read.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "the blob never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    bob.send(&mut w, &ClientMessage::FetchBlob { blob_id: blob });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        w.pump(&mut [&mut alice, &mut bob]);
        if let Some(bytes) = bob.blob_received(&blob) {
            assert_eq!(bytes, notes);
            break;
        }
        assert!(
            !bob.inbox.iter().any(|m| matches!(m,
                ServerMessage::BlobMissing { blob_id } if *blob_id == blob)),
            "the server says the blob is missing"
        );
        assert!(Instant::now() < deadline, "the blob never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Someone else's blob cannot be posted by hash.
    bob.send(
        &mut w,
        &ClientMessage::SendFile {
            conv: ConvRef::General,
            blob_id: blob,
            client_ref: 77,
        },
    );
    let reason = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::MessageFailed {
            client_ref: 77,
            reason,
        } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, DmFailReason::NoSuchBlob);

    // …but the same bytes, sent in full, may be posted by anyone: the
    // server asks for the transfer (proof of possession) instead of
    // answering "already here" and then refusing the post (review P0-4).
    bob.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 40,
            name: "notes.txt".into(),
            mime: "text/plain".into(),
            size: u32::try_from(notes.len()).unwrap(),
            sha256: blob,
        },
    );
    let needed = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UploadAccept {
            upload_id: 40,
            needed,
        } => Some(*needed),
        _ => None,
    });
    assert!(needed, "someone else's file is sent in full once");
    let (_, reposted) = post_file(
        &mut bob,
        &mut w,
        &mut [&mut alice],
        41,
        "notes.txt",
        "text/plain",
        &notes,
        ConvRef::General,
    )
    .expect("the second poster may post it");
    assert_eq!(reposted, blob);

    // Guests do not upload files.
    let mut guest = w.connect("guest");
    guest.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 3,
            name: "x.bin".into(),
            mime: "application/octet-stream".into(),
            size: 10,
            sha256: [1u8; 32],
        },
    );
    let reason = guest.wait_for(&mut w, &mut [&mut alice, &mut bob], |m| match m {
        ServerMessage::UploadRejected {
            upload_id: 3,
            reason,
        } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, UploadRejectReason::NoPermission);
    // The refusal names the upload restriction. It used to say
    // General's text kind, and the client then locked the guest's
    // chat until a reconnect (review P0-2).
    assert!(guest.inbox.iter().any(|m| matches!(
        m,
        ServerMessage::Restricted {
            kind: RestrictionKind::Upload,
            ..
        }
    )));
    assert!(!guest.inbox.iter().any(|m| matches!(
        m,
        ServerMessage::Restricted {
            kind: RestrictionKind::GeneralText,
            ..
        }
    )));

    // Over the per-file cap: refused at announce.
    alice.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 4,
            name: "huge.iso".into(),
            mime: "application/octet-stream".into(),
            size: u32::MAX,
            sha256: [2u8; 32],
        },
    );
    let reason = alice.wait_for(&mut w, &mut [&mut bob, &mut guest], |m| match m {
        ServerMessage::UploadRejected {
            upload_id: 4,
            reason,
        } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, UploadRejectReason::TooLarge);

    // A newcomer's replay carries the card, not an empty line.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut carol = w.connect_member("carol");
    let replayed = carol.wait_for(
        &mut w,
        &mut [&mut alice, &mut bob, &mut guest],
        |m| match m {
            ServerMessage::HistoryAttachments {
                conv: ConvRef::General,
                items,
            } => items
                .iter()
                .find(|(id, _)| *id == msg_id)
                .map(|(_, a)| a.clone()),
            _ => None,
        },
    );
    assert_eq!(replayed.name, "notes.txt");
    assert!(carol.inbox.iter().any(|m| matches!(m,
        ServerMessage::History { messages, .. }
            if messages.iter().any(|h| h.id == msg_id && h.text.is_empty()))));

    // Deleting the message lets go of its reference to the blob (bob's
    // repost of the same bytes still holds one).
    alice.send(&mut w, &ClientMessage::DeleteMessage { msg_id });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        w.pump(&mut [&mut alice, &mut bob, &mut guest, &mut carol]);
        if w.server()
            .policy()
            .blobs
            .get(&blob)
            .is_some_and(|b| b.refs == 1)
        {
            break;
        }
        assert!(Instant::now() < deadline, "the reference never dropped");
    }
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_goes_into_a_dm_like_a_message() {
    let dir = temp_dir("dm-files");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    let bob_user = bob.key.user_id();
    let (msg_id, blob) = post_file(
        &mut alice,
        &mut w,
        &mut [&mut bob],
        9,
        "photo.raw",
        "image/x-raw",
        &[7u8; 5_000],
        ConvRef::Dm(bob_user),
    )
    .expect("posted");
    let (from, card) = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::FileMessage {
            conv: ConvRef::Dm(_),
            from,
            msg,
            attachment,
        } if msg.id == msg_id => Some((*from, attachment.clone())),
        _ => None,
    });
    assert_eq!(from, alice_user);
    assert_eq!(card.blob_id, blob);
    // Not an image: the declared type is kept when sane. `image/x-raw`
    // is a well-formed type that does not sniff as an image.
    assert_eq!(card.mime, "image/x-raw");
    // Nobody else saw it.
    let mut carol = w.connect_member("carol");
    w.settle(
        &mut [&mut alice, &mut bob, &mut carol],
        Duration::from_millis(500),
    );
    assert!(!carol
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::FileMessage { .. })));
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_panel_manages_roles_and_blobs_and_reads_the_audit() {
    let dir = temp_dir("panel");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");

    // A role from the panel: created with the same rules as the CLI,
    // announced to the room, edited, then deleted.
    admin.admin(
        &mut w,
        1,
        AdminCommand::CreateRole {
            name: "Streamer".into(),
            permissions: permissions::MEMBER_PERMISSIONS,
            icon: String::new(),
            color: 0,
        },
    );
    assert!(matches!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Err(AdminError::Invalid(_))
    ));
    admin.admin(
        &mut w,
        2,
        AdminCommand::CreateRole {
            name: "streamer".into(),
            permissions: permissions::MEMBER_PERMISSIONS | permissions::SHARE,
            icon: "zap".into(),
            color: 0x007E_D321,
        },
    );
    let Ok(AdminOutcome::RoleCreated(id)) = admin.admin_result(&mut w, &mut [&mut alice], 2) else {
        panic!("role not created");
    };
    alice.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::RoleList { roles } if roles.iter().any(|r| r.id == id) => Some(()),
        _ => None,
    });
    admin.admin(
        &mut w,
        3,
        AdminCommand::EditRole {
            role: id,
            name: Some("caster".into()),
            permissions: None,
            icon: None,
            color: None,
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 3),
        Ok(AdminOutcome::Done)
    );
    assert_eq!(w.server().policy().roles[&id].name, "caster");
    admin.admin(
        &mut w,
        4,
        AdminCommand::EditRole {
            role: ROLE_MEMBER,
            name: Some("members".into()),
            permissions: None,
            icon: None,
            color: None,
        },
    );
    assert!(
        matches!(
            admin.admin_result(&mut w, &mut [&mut alice], 4),
            Err(AdminError::Invalid(_))
        ),
        "built-ins stay"
    );
    admin.admin(&mut w, 5, AdminCommand::DeleteRole { role: id });
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 5),
        Ok(AdminOutcome::Done)
    );
    assert!(!w.server().policy().roles.contains_key(&id));

    // Stats and the audit are the admin's; a member gets refused.
    admin.admin(&mut w, 6, AdminCommand::ServerStats);
    let Ok(AdminOutcome::Stats(stats)) = admin.admin_result(&mut w, &mut [&mut alice], 6) else {
        panic!("no stats");
    };
    assert_eq!(stats.online, 2);
    assert!(stats.persistent);
    alice.admin(&mut w, 7, AdminCommand::ServerStats);
    assert_eq!(
        alice.admin_result(&mut w, &mut [&mut admin], 7),
        Err(AdminError::NoPermission)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    admin.admin(
        &mut w,
        8,
        AdminCommand::AuditLog {
            before: None,
            limit: 50,
        },
    );
    let Ok(AdminOutcome::Audit(entries)) = admin.admin_result(&mut w, &mut [&mut alice], 8) else {
        panic!("no audit");
    };
    assert!(entries
        .iter()
        .any(|e| e.action == "role.create" && e.actor.as_deref() == Some("mpw")));
    assert!(entries.iter().any(|e| e.action == "role.delete"));
    assert!(
        entries.windows(2).all(|p| p[0].id > p[1].id),
        "newest first"
    );

    // Blobs: listed with their uploader, deleted from the panel.
    let (_, blob) = post_file(
        &mut alice,
        &mut w,
        &mut [&mut admin],
        20,
        "readme.md",
        "text/plain",
        b"hello there",
        ConvRef::General,
    )
    .expect("posted");
    admin.admin(&mut w, 9, AdminCommand::ListBlobs);
    let Ok(AdminOutcome::Blobs(blobs)) = admin.admin_result(&mut w, &mut [&mut alice], 9) else {
        panic!("no blobs");
    };
    let listed = blobs
        .iter()
        .find(|b| b.blob_id == blob)
        .expect("the file is listed");
    assert_eq!(listed.created_by.as_deref(), Some("alice"));
    assert_eq!(listed.refs, 1);
    admin.admin(&mut w, 10, AdminCommand::DeleteBlob { blob_id: blob });
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 10),
        Ok(AdminOutcome::Done)
    );
    assert!(!w.server().policy().blobs.contains_key(&blob));
    alice.send(&mut w, &ClientMessage::FetchBlob { blob_id: blob });
    alice.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::BlobMissing { blob_id } if *blob_id == blob => Some(()),
        _ => None,
    });
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disconnect_reaches_anyone_and_kick_spares_admins() {
    use server::persistence::{cli_role_create, cli_set_roles};
    let dir = temp_dir("mod-rule");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut mod_ = w.connect_member("mod");
    let mut peer = w.connect_member("peer");
    let admin_user = admin.key.user_id();
    let mod_user = mod_.key.user_id();
    let peer_user = peer.key.user_id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    // A member-level custom role with the two powers.
    let db = dir.join("baucord.db");
    let powers = permissions::MEMBER_PERMISSIONS | permissions::DISCONNECT | permissions::KICK;
    let id = cli_role_create(&db, "bouncer", powers, "", 0).expect("role");
    cli_set_roles(&db, &mod_user, &[id]).expect("grant");
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut admin, &mut mod_, &mut peer]);
        if mod_.inbox.iter().any(
            |m| matches!(m, ServerMessage::Session { permissions, .. } if *permissions == powers),
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the role never reached the session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Kicking an equal (a member, rank 10 vs the bouncer's 15 — and a
    // second bouncer would be equal): fine. The admin, holding
    // `ADMIN_CHANNEL`, sees the line as it happens (spec §8).
    mod_.admin(
        &mut w,
        3,
        AdminCommand::Kick {
            target: AdminTarget::User(peer_user),
            reason: "bye".into(),
        },
    );
    assert_eq!(
        mod_.admin_result(&mut w, &mut [&mut admin], 3),
        Ok(AdminOutcome::Done)
    );
    let entry = admin.wait_for(&mut w, &mut [&mut mod_], |m| match m {
        ServerMessage::AuditLine { entry } if entry.action == "user.kick" => Some(entry.clone()),
        _ => None,
    });
    assert_eq!(entry.actor.as_deref(), Some("mod"));
    assert_eq!(
        entry.target.as_deref(),
        Some("peer"),
        "targets come as names"
    );
    assert!(
        !mod_
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::AuditLine { .. })),
        "the bouncer lacks the channel permission"
    );
    // Kicking the admin: the one protection.
    mod_.admin(
        &mut w,
        2,
        AdminCommand::Kick {
            target: AdminTarget::User(admin_user),
            reason: String::new(),
        },
    );
    assert_eq!(
        mod_.admin_result(&mut w, &mut [&mut admin], 2),
        Err(AdminError::Outranked)
    );
    // Disconnecting the admin: allowed, the rank rule does not apply.
    mod_.admin(
        &mut w,
        1,
        AdminCommand::Disconnect {
            target: AdminTarget::User(admin_user),
            reason: "reconnect please".into(),
        },
    );
    assert_eq!(
        mod_.admin_result(&mut w, &mut [&mut admin], 1),
        Ok(AdminOutcome::Done)
    );
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn everyone_sees_everyones_progress() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    let alice_id = alice.id;
    let bob_id = bob.id;
    // The newcomer gets the roster's standing (level 1, nothing yet),
    // itself included; the room gets the newcomer's.
    bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserProgress {
            client_id,
            xp,
            level,
        } if *client_id == alice_id => (*xp == 0 && *level == 1).then_some(()),
        _ => None,
    });
    bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserProgress { client_id, .. } if *client_id == bob_id => Some(()),
        _ => None,
    });
    alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UserProgress { client_id, .. } if *client_id == bob_id => Some(()),
        _ => None,
    });
    // A minute of alice talking with bob there: 6 xp, and bob sees it
    // move (no level-up involved).
    alice.send(&mut w, &ClientMessage::Speaking(true));
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(61));
    let (xp, level) = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserProgress {
            client_id,
            xp,
            level,
        } if *client_id == alice_id && *xp > 0 => Some((*xp, *level)),
        _ => None,
    });
    assert_eq!((xp, level), (6, 1));
}

#[test]
fn a_purge_removes_the_person_and_their_messages() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut troll = w.connect("troll");
    let troll_user = troll.key.user_id();
    troll.send(&mut w, &ClientMessage::Chat("garbage".to_string()));
    admin.wait_for(&mut w, &mut [&mut troll], |m| {
        matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "garbage").then_some(())
    });

    admin.admin(
        &mut w,
        1,
        AdminCommand::PurgeUser {
            target: AdminTarget::Name("troll".to_string()),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut troll], 1),
        Ok(AdminOutcome::Done)
    );
    assert!(!w.server().policy().users.contains_key(&troll_user));
    let mut carol = w.connect("carol");
    w.settle(&mut [&mut carol, &mut admin], Duration::from_millis(300));
    assert!(carol.inbox.iter().all(|m| match m {
        ServerMessage::History { messages, .. } => messages.iter().all(|x| x.text != "garbage"),
        _ => true,
    }));
}

#[test]
fn organizers_make_channels_that_gate_by_role_and_evaporate_when_empty() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.user_channels.temporary_after_min = 1;
    let mut w = World::new(config);
    let org_code = w.invite(protocol::permissions::ROLE_ORGANIZER);
    let mut org = w.connect_as("org", Auth::Invite { code: org_code });
    let mut guest = w.connect("guest");
    let org_id = org.id;

    // A guest may not create; an organizer may, and lands in it.
    let spec = ChannelSpec {
        name: "Backroom".to_string(),
        password: None,
        user_limit: None,
        min_role: Some(ROLE_MEMBER),
        permanent: false,
        afk_after_min: None,
    };
    guest.send(&mut w, &ClientMessage::CreateChannel(spec.clone()));
    let reason = guest.wait_for(&mut w, &mut [&mut org], |m| match m {
        ServerMessage::ChannelDenied { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, ChannelUiDenyReason::NoPermission);
    org.send(&mut w, &ClientMessage::CreateChannel(spec));
    let channels = org.wait_for(&mut w, &mut [&mut guest], |m| match m {
        ServerMessage::ChannelListUpdated { channels, .. } => Some(channels.clone()),
        _ => None,
    });
    assert_eq!(channels.len(), 3, "Lobby, Backroom, and the lobby entry");
    assert_eq!(channels[1].name, "Backroom");
    let backroom = channels[1].id;
    org.wait_for(&mut w, &mut [&mut guest], |m| {
        matches!(m, ServerMessage::UserMovedChannel { client_id, channel_id }
            if *client_id == org_id && *channel_id == backroom)
        .then_some(())
    });
    assert_eq!(w.server().channel_of(org_id), Some(backroom));
    let org_user = org.key.user_id();
    // The lobby entry held this id before the Backroom was created,
    // and its `ChannelDetails` (no owner) is still in the inbox: the
    // Backroom's own details are the ones with an owner.
    let details = guest.wait_for(&mut w, &mut [&mut org], |m| match m {
        ServerMessage::ChannelDetails {
            channel_id,
            owner: Some(owner),
            min_role,
            permanent,
            ..
        } if *channel_id == backroom => Some((Some(*owner), *min_role, *permanent)),
        _ => None,
    });
    assert_eq!(details, (Some(org_user), Some(ROLE_MEMBER), false));

    // The gate: a guest is refused, a member walks in.
    guest.join(&mut w, backroom, None);
    let reason = guest.wait_for(&mut w, &mut [&mut org], |m| match m {
        ServerMessage::JoinChannelDenied { reason, .. } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, ChannelDenyReason::NotPermitted);
    let member_code = w.invite(ROLE_MEMBER);
    let mut member = w.connect_as("member", Auth::Invite { code: member_code });
    let member_id = member.id;
    member.join(&mut w, backroom, None);
    member.wait_for(&mut w, &mut [&mut org, &mut guest], |m| {
        matches!(m, ServerMessage::UserMovedChannel { client_id, channel_id }
            if *client_id == member_id && *channel_id == backroom)
        .then_some(())
    });

    // The owner sends the member back; everyone leaves; a minute later
    // the temporary channel is gone and the list is back to one.
    let member_user = member.key.user_id();
    org.send(
        &mut w,
        &ClientMessage::ChannelKick {
            channel_id: backroom,
            user_id: member_user,
        },
    );
    // "Out" is the lobby now (spec §12, 1.1.5): the member alone is
    // told where they went, and to the others they left.
    let lobby = channels.last().expect("the lobby is last").id;
    member.wait_for(&mut w, &mut [&mut org, &mut guest], |m| {
        matches!(m, ServerMessage::UserMovedChannel { client_id, channel_id }
            if *client_id == member_id && *channel_id == lobby)
        .then_some(())
    });
    org.wait_for(&mut w, &mut [&mut guest, &mut member], |m| {
        matches!(m, ServerMessage::UserLeft { client_id } if *client_id == member_id).then_some(())
    });
    org.join(&mut w, 0, None);
    w.settle(
        &mut [&mut org, &mut guest, &mut member],
        Duration::from_secs(35),
    );
    assert_eq!(
        w.server().policy().channels.len(),
        1,
        "empty, but not yet long enough"
    );
    w.advance_wall(Duration::from_mins(2));
    w.settle(
        &mut [&mut org, &mut guest, &mut member],
        Duration::from_secs(35),
    );
    let channels = guest.wait_for(&mut w, &mut [&mut org, &mut member], |m| match m {
        ServerMessage::ChannelListUpdated { channels, .. } if channels.len() == 2 => {
            Some(channels.clone())
        }
        _ => None,
    });
    assert_eq!(channels[0].name, "Lobby");
    assert!(w.server().policy().channels.is_empty());
}

#[test]
fn the_owner_edits_or_deletes_and_an_admin_can_too_but_config_channels_stay() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let org_code = w.invite(protocol::permissions::ROLE_ORGANIZER);
    let mut org = w.connect_as("org", Auth::Invite { code: org_code });
    let mut spec = ChannelSpec {
        name: "Games".to_string(),
        password: None,
        user_limit: None,
        min_role: None,
        permanent: true,
        afk_after_min: None,
    };
    org.send(&mut w, &ClientMessage::CreateChannel(spec.clone()));
    let games = org.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::ChannelListUpdated { channels, .. } if channels.len() == 3 => {
            Some(channels[1].id)
        }
        _ => None,
    });

    // The config channel cannot be deleted by anyone.
    admin.send(&mut w, &ClientMessage::DeleteChannel { channel_id: 0 });
    let reason = admin.wait_for(&mut w, &mut [&mut org], |m| match m {
        ServerMessage::ChannelDenied { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, ChannelUiDenyReason::ConfigOwned);

    // A rename by the owner, a second organizer refused, a delete by
    // the admin.
    spec.name = "Board games".to_string();
    org.send(
        &mut w,
        &ClientMessage::EditChannel {
            channel_id: games,
            spec,
        },
    );
    org.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::ChannelListUpdated { channels, .. }
            if channels.get(1).is_some_and(|c| c.name == "Board games") =>
        {
            Some(())
        }
        _ => None,
    });
    let other_code = w.invite(protocol::permissions::ROLE_ORGANIZER);
    let mut other = w.connect_as("other", Auth::Invite { code: other_code });
    other.send(&mut w, &ClientMessage::DeleteChannel { channel_id: games });
    let reason = other.wait_for(&mut w, &mut [&mut org, &mut admin], |m| match m {
        ServerMessage::ChannelDenied { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, ChannelUiDenyReason::NoPermission);
    admin.send(&mut w, &ClientMessage::DeleteChannel { channel_id: games });
    admin.wait_for(&mut w, &mut [&mut org, &mut other], |m| match m {
        ServerMessage::ChannelListUpdated { channels, .. } if channels.len() == 2 => Some(()),
        _ => None,
    });
}

#[test]
fn idle_members_are_moved_to_the_afk_channel_where_voice_goes_nowhere() {
    let mut config = test_config(vec![
        open_channel("Lobby", true),
        open_channel("AFK", false),
    ]);
    config.user_channels.afk_channel = Some("AFK".to_string());
    config.user_channels.afk_after_min = 1;
    let mut w = World::new(config);
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    // Alice keeps busy; bob does nothing. The channel sweep runs every
    // 30 s: at 60 s bob is a few hundred ms short of a minute idle (he
    // was admitted after the server started), at 90 s he is well past
    // it, and alice's last activity is 28 s old.
    for _ in 0..3 {
        alice.send(&mut w, &ClientMessage::Speaking(true));
        w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(31));
    }
    let after = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::MovedIdle { after_min } => Some(*after_min),
        _ => None,
    });
    assert_eq!(after, 1);
    assert_eq!(w.server().channel_of(bob.id), Some(1));
    assert_eq!(w.server().channel_of(alice.id), Some(0));

    // Voice from the AFK channel goes nowhere.
    let pkt = protocol::pack_voice(1, 20, &[0xAB; 60]).expect("voice");
    let now = w.net.now();
    w.net
        .driver(bob.node)
        .send(now, bob.peer, Lane::Voice, &pkt)
        .expect("send voice");
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(300));
    assert!(w
        .net
        .driver(alice.node)
        .recv(alice.peer, Lane::Voice)
        .is_none());
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

/// `server user purge` while the server runs (review P2-8): the
/// person's messages leave the replay ring and everyone's screen with
/// the reload, not after 200 newer messages pushed them out.
#[test]
fn a_purge_by_the_cli_takes_the_messages_off_the_live_server() {
    let dir = temp_dir("cli-purge");
    let db = dir.join("baucord.db");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let mut troll = w.connect("troll");
    let troll_user = troll.key.user_id();
    troll.send(&mut w, &ClientMessage::Chat("garbage".to_string()));
    alice.wait_for(&mut w, &mut [&mut troll], |m| {
        matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "garbage").then_some(())
    });
    // On disk before the CLI looks for the account.
    let deadline = Instant::now() + Duration::from_secs(6);
    while server::persistence::cli_find_user(&db, "troll").is_err()
        || stored_message_count(&dir) == 0
    {
        assert!(Instant::now() < deadline, "the message never reached disk");
        std::thread::sleep(Duration::from_millis(20));
        w.pump(&mut [&mut alice, &mut troll]);
    }

    server::persistence::cli_purge(&db, &troll_user).expect("purge");
    // The persistence thread polls `data_version` every 2 s of real time.
    let deadline = Instant::now() + Duration::from_secs(6);
    while !alice
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::MessageDeleted { .. }))
    {
        assert!(
            Instant::now() < deadline,
            "the purged message stayed on screen"
        );
        std::thread::sleep(Duration::from_millis(50));
        w.pump(&mut [&mut alice, &mut troll]);
    }
    let mut carol = w.connect("carol");
    w.settle(&mut [&mut carol, &mut alice], Duration::from_millis(300));
    assert!(
        carol.inbox.iter().all(|m| match m {
            ServerMessage::History { messages, .. } => messages.iter().all(|x| x.text != "garbage"),
            _ => true,
        }),
        "a newcomer is still replayed the purged message"
    );
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A key or an address names nobody (review P2-10): it can only be
/// banned from the server, and that takes the ban permission.
#[test]
fn a_key_or_an_address_can_only_be_banned_and_only_with_the_ban_permission() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let restrict = |kind| AdminCommand::Restrict {
        target: AdminTarget::Address("10.9.9.9".to_string()),
        kind,
        duration_secs: None,
        reason: String::new(),
    };

    admin.admin(&mut w, 1, restrict(RestrictionKind::GeneralText));
    assert!(matches!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Err(AdminError::Invalid(_))
    ));

    // A role that may restrict but not ban.
    admin.admin(
        &mut w,
        2,
        AdminCommand::CreateRole {
            name: "warden".into(),
            permissions: permissions::MEMBER_PERMISSIONS | permissions::RESTRICT,
            icon: String::new(),
            color: 0,
        },
    );
    let Ok(AdminOutcome::RoleCreated(warden)) = admin.admin_result(&mut w, &mut [&mut alice], 2)
    else {
        panic!("role not created");
    };
    admin.admin(
        &mut w,
        3,
        AdminCommand::SetRoles {
            target: AdminTarget::Name("alice".to_string()),
            roles: vec![ROLE_MEMBER, warden],
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 3),
        Ok(AdminOutcome::Done)
    );
    alice.admin(&mut w, 4, restrict(RestrictionKind::Server));
    assert_eq!(
        alice.admin_result(&mut w, &mut [&mut admin], 4),
        Err(AdminError::NoPermission)
    );
    assert!(w.server().policy().restrictions.is_empty());

    admin.admin(&mut w, 5, restrict(RestrictionKind::Server));
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 5),
        Ok(AdminOutcome::Done)
    );
    assert_eq!(w.server().policy().restrictions.len(), 1);
}

/// Keeping a person out of the server is a ban by whatever command
/// asks for it: a role that may restrict and not ban gets its mutes and
/// no more (the rest of review P2-10).
#[test]
fn a_server_restriction_on_a_person_takes_the_ban_permission() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let mut guest = w.connect("guest");
    admin.admin(
        &mut w,
        1,
        AdminCommand::CreateRole {
            name: "warden".into(),
            permissions: permissions::MEMBER_PERMISSIONS | permissions::RESTRICT,
            icon: String::new(),
            color: 0,
        },
    );
    let Ok(AdminOutcome::RoleCreated(warden)) =
        admin.admin_result(&mut w, &mut [&mut alice, &mut guest], 1)
    else {
        panic!("role not created");
    };
    admin.admin(
        &mut w,
        2,
        AdminCommand::SetRoles {
            target: AdminTarget::Name("alice".to_string()),
            roles: vec![ROLE_MEMBER, warden],
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut guest], 2),
        Ok(AdminOutcome::Done)
    );
    let on_guest = |kind| AdminCommand::Restrict {
        target: AdminTarget::Name("guest".to_string()),
        kind,
        duration_secs: None,
        reason: String::new(),
    };
    alice.admin(&mut w, 3, on_guest(RestrictionKind::Server));
    assert_eq!(
        alice.admin_result(&mut w, &mut [&mut admin, &mut guest], 3),
        Err(AdminError::NoPermission)
    );
    assert!(w.server().policy().restrictions.is_empty());
    // What the role is for still works.
    alice.admin(&mut w, 4, on_guest(RestrictionKind::GeneralText));
    assert_eq!(
        alice.admin_result(&mut w, &mut [&mut admin, &mut guest], 4),
        Ok(AdminOutcome::Done)
    );
}

/// A ban may name an equal (spec §12: only an administrator is out of
/// reach), and the person named leaves at once. The rank rule that
/// protects whoever else shares their key or address used to protect
/// them too: the ban was stored, the answer was `Done`, and they stayed
/// in the room until they next reconnected.
#[test]
fn a_ban_of_an_equal_removes_them_at_once() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    admin.admin(
        &mut w,
        1,
        AdminCommand::CreateRole {
            name: "bouncer".into(),
            permissions: permissions::MEMBER_PERMISSIONS | permissions::BAN,
            icon: String::new(),
            color: 0,
        },
    );
    let Ok(AdminOutcome::RoleCreated(bouncer)) =
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 1)
    else {
        panic!("role not created");
    };
    for (request, name) in [(2, "alice"), (3, "bob")] {
        admin.admin(
            &mut w,
            request,
            AdminCommand::SetRoles {
                target: AdminTarget::Name(name.to_string()),
                roles: vec![ROLE_MEMBER, bouncer],
            },
        );
        assert_eq!(
            admin.admin_result(&mut w, &mut [&mut alice, &mut bob], request),
            Ok(AdminOutcome::Done)
        );
    }
    alice.admin(
        &mut w,
        4,
        AdminCommand::Ban {
            target: AdminTarget::Name("bob".to_string()),
            duration_secs: None,
            reason: "enough".to_string(),
        },
    );
    bob.wait_for(&mut w, &mut [&mut admin, &mut alice], |m| match m {
        ServerMessage::Removed {
            kind: RemovedKind::Banned { until: None },
            reason,
            ..
        } if reason == "enough" => Some(()),
        _ => None,
    });
    // The administrator on the same address is still there, and still
    // out of reach.
    alice.admin(
        &mut w,
        5,
        AdminCommand::Ban {
            target: AdminTarget::Name("mpw".to_string()),
            duration_secs: None,
            reason: String::new(),
        },
    );
    assert_eq!(
        alice.admin_result(&mut w, &mut [&mut admin], 5),
        Err(AdminError::Outranked)
    );
}

/// A full server still shakes hands, and says why it refuses. The
/// transport used to count the unadmitted against `max_clients` too,
/// so the next connection was refused before it could say Hello and
/// `ServerFull` reached nobody (review P3-5).
#[test]
fn server_full_is_refused_at_hello() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.network.max_clients = 1;
    let mut w = World::new(config);
    let _a = w.connect("alice");
    let code = w.guest_code.clone();
    assert_eq!(
        w.try_connect("bob", Auth::Invite { code }).unwrap_err(),
        RefusedReason::ServerFull
    );
}

/// The seat a user holds is theirs: back after a blackout, with the
/// old session still on the server's books, their new connection
/// replaces it even on a full server.
#[test]
fn a_returning_user_replaces_their_session_on_a_full_server() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.network.max_clients = 1;
    let mut w = World::new(config);
    let mut old = w.connect("alice");
    let key = Arc::clone(&old.key);
    let mut new = w
        .try_connect_with_key("alice", Auth::Key, key)
        .expect("the same user gets back in");
    assert_ne!(new.id, old.id);
    old.wait_for(&mut w, &mut [&mut new], |m| {
        matches!(
            m,
            ServerMessage::Removed {
                kind: RemovedKind::Replaced,
                ..
            }
        )
        .then_some(())
    });
}

/// One address opening connections in a loop, never saying Hello: it
/// holds its share of the waiting room and no more, and someone else
/// still gets in (plan SEC-2).
#[test]
fn one_address_cannot_fill_the_waiting_room() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut hostile: Vec<TestClient> = (0..30u16)
        .map(|i| {
            let from: SocketAddr = format!("10.9.9.9:{}", 7000 + i).parse().expect("addr");
            w.open_attempt_from(from)
        })
        .collect();
    {
        let mut refs: Vec<&mut TestClient> = hostile.iter_mut().collect();
        w.settle(&mut refs, Duration::from_secs(1));
    }
    let connected = hostile
        .iter()
        .filter(|c| w.net.driver(c.node).is_connected(c.peer))
        .count();
    assert_eq!(connected, 8, "its share of the room, the rest refused");

    let alice = w.connect("alice");
    assert_ne!(alice.id, 0);
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

    // Two from the config plus the lobby the server appends (1.1.5).
    assert_eq!(channels.len(), 3);
    assert_eq!(channels[0].name, "Lobby");
    assert!(!channels[0].has_password);
    assert!(channels[1].has_password);
    assert_eq!(channels[1].user_limit, Some(8));
    assert_eq!(channels[2].name, "", "the lobby is last and unnamed");
    // `Init` places us in the lobby (the appended entry); the harness
    // then joined room 0 on our behalf.
    assert_eq!(memberships.get(&my_id), Some(&2));
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
    let mut sharer = w.connect_member("sharer");
    let mut viewer = w.connect_member("viewer");
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

/// The lobby is shown no shares, and cannot watch one either: an
/// unseen viewer there still counted in the sharer's stream health.
/// A stop from someone who is not sharing reaches nobody.
#[test]
fn the_lobby_cannot_watch_and_a_stray_stop_says_nothing() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut sharer = w.connect_member("sharer");
    let mut other = w.connect_member("other");
    let sharer_id = sharer.id;
    let other_id = other.id;
    let code = w.member_code.clone();
    let mut lurker = w.connect_lobby_with_key(
        "lurker",
        Auth::Invite { code },
        Arc::new(ClientKey::generate()),
    );
    sharer.send(
        &mut w,
        &ClientMessage::ScreenShareStart {
            width: 1280,
            height: 720,
            scope: ShareScope::Everyone,
        },
    );
    other.wait_for(&mut w, &mut [&mut sharer, &mut lurker], |m| {
        matches!(m, ServerMessage::ScreenShareStarted { client_id, .. } if *client_id == sharer_id)
            .then_some(())
    });
    lurker.send(
        &mut w,
        &ClientMessage::SubscribeScreen {
            target_id: sharer_id,
        },
    );
    lurker.wait_for(&mut w, &mut [&mut sharer, &mut other], |m| {
        matches!(
            m,
            ServerMessage::SubscribeDenied {
                reason: DenyReason::NotInChannel,
                ..
            }
        )
        .then_some(())
    });

    other.send(&mut w, &ClientMessage::ScreenShareStop);
    w.settle(
        &mut [&mut sharer, &mut other, &mut lurker],
        Duration::from_millis(500),
    );
    assert!(
        !sharer.inbox.iter().any(|m| matches!(m,
            ServerMessage::ScreenShareStopped { client_id } if *client_id == other_id)),
        "a stop with no share behind it went out"
    );
}

/// What a session does in the lobby stays there (review P2-1): its
/// speaking edges, its mute, a rename and a role change used to reach
/// every client, each one naming the session nobody was told about.
#[test]
fn a_lobby_session_keeps_its_state_to_itself() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let mut bob = w.connect_lobby("bob");
    let bob_id = bob.id;
    let bob_user = bob.key.user_id();
    bob.send(&mut w, &ClientMessage::Speaking(true));
    bob.send(
        &mut w,
        &ClientMessage::AudioState {
            muted: true,
            deafened: false,
        },
    );
    bob.send(
        &mut w,
        &ClientMessage::SetNickname {
            name: "robert".to_string(),
        },
    );
    admin.admin(
        &mut w,
        9,
        AdminCommand::SetRoles {
            target: AdminTarget::User(bob_user),
            roles: vec![ROLE_MEMBER],
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 9),
        Ok(AdminOutcome::Done)
    );
    w.settle(
        &mut [&mut admin, &mut alice, &mut bob],
        Duration::from_secs(1),
    );
    let about_bob = |m: &ServerMessage| match m {
        ServerMessage::Speaking { client_id, .. }
        | ServerMessage::AudioState { client_id, .. }
        | ServerMessage::UserRenamed { client_id, .. }
        | ServerMessage::UserDetails { client_id, .. } => *client_id == bob_id,
        _ => false,
    };
    assert!(
        !alice.inbox.iter().any(about_bob),
        "a lobby session's state reached someone in a room: {:?}",
        alice
            .inbox
            .iter()
            .filter(|m| about_bob(m))
            .collect::<Vec<_>>()
    );
    assert!(
        bob.inbox.iter().any(|m| matches!(m,
            ServerMessage::UserRenamed { client_id, name } if *client_id == bob_id && name == "robert")),
        "the lobby session still hears its own rename"
    );
}

#[test]
fn the_lobby_hides_a_session_until_it_joins_a_room() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let mut bob = w.connect_lobby("bob");
    let bob_id = bob.id;

    // Nothing about bob reaches alice while he sits in the lobby, and
    // bob's own `Init` lists the rooms' people.
    w.settle(
        &mut [&mut admin, &mut alice, &mut bob],
        Duration::from_secs(1),
    );
    assert!(
        !alice.inbox.iter().any(|m| matches!(m,
            ServerMessage::UserJoined { client_id, .. } if *client_id == bob_id)),
        "a lobby member is announced to nobody"
    );
    let (usernames, channels) = bob
        .inbox
        .iter()
        .find_map(|m| match m {
            ServerMessage::Init {
                usernames,
                channels,
                ..
            } => Some((usernames.clone(), channels.clone())),
            _ => None,
        })
        .expect("init");
    assert!(
        usernames.contains_key(&alice.id),
        "the lobby sees the rooms"
    );
    let lobby = channels.last().expect("the lobby is last").id;

    // The first join is the announcement, and it carries the room.
    bob.join(&mut w, 0, None);
    let room = alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| match m {
        ServerMessage::UserJoined {
            client_id,
            channel_id,
            ..
        } if *client_id == bob_id => Some(*channel_id),
        _ => None,
    });
    assert_eq!(room, 0);

    // Back to the lobby by an admin's hand: to alice, bob left.
    let bob_user = bob.key.user_id();
    admin.admin(
        &mut w,
        1,
        AdminCommand::MoveUser {
            target: AdminTarget::User(bob_user),
            channel_id: lobby,
        },
    );
    alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| {
        matches!(m, ServerMessage::UserLeft { client_id } if *client_id == bob_id).then_some(())
    });
}

#[test]
fn config_validation_rejects_bad_channel_lists() {
    // No `default: true` anywhere is fine since 1.1.5 (the lobby is
    // implicit); the index returned is the lobby's, after the list.
    let config = test_config(vec![open_channel("A", false)]);
    assert_eq!(config.validate_channels().expect("valid"), 1);
    // Two defaults: accepted and ignored, not an error.
    let config = test_config(vec![open_channel("A", true), open_channel("B", true)]);
    assert_eq!(config.validate_channels().expect("valid"), 2);
    // A password or a limit on a flagged channel: an ordinary room.
    let config = test_config(vec![ChannelSettings {
        name: "A".to_string(),
        default: true,
        password: Some("x".to_string()),
        user_limit: Some(4),
    }]);
    assert!(config.validate_channels().is_ok());
    // Empty list.
    let config = test_config(vec![]);
    assert!(config.validate_channels().is_err());
    // Duplicate names.
    let config = test_config(vec![open_channel("A", true), open_channel("A", false)]);
    assert!(config.validate_channels().is_err());
    // Missing key in YAML → a single 'General', plus the lobby at 1.
    let config: Config = serde_yaml::from_str("server:\n  name: test\n").expect("yaml");
    assert_eq!(config.validate_channels().expect("valid"), 1);
    assert_eq!(config.channels[0].name, "General");
    // A 1.0 `authentication:` section and a stale 0.5 key both still
    // parse (ignored: the field is gone, review 2026-10), and the
    // section is never written back.
    let config: Config = serde_yaml::from_str(
        "authentication:\n  secure: true\n  password: pw\nnetwork:\n  bytes_per_tick: 4194304\n",
    )
    .expect("yaml");
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
    let lobby = channels.last().expect("the lobby is last").id;
    assert_eq!(
        memberships.get(&a.id),
        Some(&lobby),
        "renamed ⇒ back to the lobby"
    );
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
    let mut sharer = w.connect_member("sharer");
    let mut viewer = w.connect_member("viewer");
    let mut lobbyist = w.connect_member("lobbyist");
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

    // Movies disappears: sharer AND viewer are sent to the lobby (spec
    // §12, 1.1.5), where nobody streams and nobody watches — the
    // stream ends for the viewer, and the lobbyist (in the "Lobby"
    // room, a real room) never learns of it.
    w.server()
        .apply_channel_config(vec![open_channel("Lobby", true)])
        .expect("valid list");

    viewer.wait_for(&mut w, &mut [&mut sharer, &mut lobbyist], |m| {
        matches!(m, ServerMessage::ScreenShareStopped { client_id } if *client_id == sharer_id)
            .then_some(())
    });
    assert!(
        !lobbyist
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::ScreenShareStarted { .. })),
        "the lobby shows nothing"
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

    // Empty / duplicate names: both rejected, nothing broadcast.
    assert!(w.server().apply_channel_config(vec![]).is_err());
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
        ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "ping" => {
            Some((c.id, c.ts))
        }
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
    config.limits.max_transfer_kb_per_minute = 100; // 100 KiB/min per client
    config.limits.max_transfer_kb_per_minute_global = 100_000; // loose
    let mut w = World::new(config);
    let mut a = w.connect_member("alice");
    let mut b = w.connect_member("bob");

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

/// One file larger than the whole per-minute budget goes when the
/// minute is empty (it used to be refused for good, as "today's
/// allowance"); the next one in that minute is refused as `Refused`,
/// not `Quota`.
#[test]
fn a_file_larger_than_the_minute_budget_goes_alone() {
    let dir = temp_dir("minute-budget");
    let mut config = persistent_config(vec![open_channel("Lobby", true)], &dir);
    config.limits.max_transfer_kb_per_minute = 1; // 1 KiB/min per client
    let mut w = World::new(config);
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");

    let big = b"three kilobytes of notes, one budget of one".repeat(70);
    assert!(big.len() > 1024);
    post_file(
        &mut alice,
        &mut w,
        &mut [&mut bob],
        1,
        "big.txt",
        "text/plain",
        &big,
        ConvRef::General,
    )
    .expect("an empty minute takes one file of any size under the cap");

    let second = b"a second file inside the same minute".repeat(10);
    alice.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 2,
            name: "second.txt".into(),
            mime: "text/plain".into(),
            size: u32::try_from(second.len()).unwrap(),
            sha256: transport::sha256::digest(&second),
        },
    );
    let reason = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UploadRejected {
            upload_id: 2,
            reason,
        } => Some(*reason),
        ServerMessage::UploadAccept { upload_id: 2, .. } => {
            panic!("the minute's budget is spent")
        }
        _ => None,
    });
    assert_eq!(reason, UploadRejectReason::Refused);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A picture whose header claims more than any client decodes is not
/// relayed: a few KB that would freeze whoever shows it (review
/// 2026-10, CLI-1).
#[test]
fn a_picture_no_client_would_show_is_not_relayed() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let mut bomb = png_blob(1, 2000);
    bomb[16..20].copy_from_slice(&16_384u32.to_be_bytes());
    bomb[20..24].copy_from_slice(&16_384u32.to_be_bytes());
    alice.send_image(&mut w, 1, &bomb);
    bob.send_image(&mut w, 2, &png_blob(2, 2000));
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(3));
    assert_eq!(alice.images_completed(), 1, "only bob's");
}

/// A file does not spend the pictures' minute: one large file filled the
/// shared server-wide window, and every picture after it was refused
/// for a minute (review 2026-10, P1-11).
#[test]
fn a_large_file_leaves_the_pictures_budget_alone() {
    let dir = temp_dir("file-vs-picture");
    let mut config = persistent_config(vec![open_channel("Lobby", true)], &dir);
    config.limits.max_transfer_kb_per_minute = 1; // 1 KiB/min per client
    config.limits.max_transfer_kb_per_minute_global = 1; // and room-wide
    let mut w = World::new(config);
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");

    let big = b"three kilobytes of notes, one budget of one".repeat(70);
    post_file(
        &mut alice,
        &mut w,
        &mut [&mut bob],
        1,
        "big.txt",
        "text/plain",
        &big,
        ConvRef::General,
    )
    .expect("files are counted apart, under a floor of the file cap");

    let picture = png_blob(7, 600);
    bob.send_image(&mut w, 2, &picture);
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(3));
    assert_eq!(alice.images_completed(), 1, "the picture went through");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The watched config's channel list applies on a live server, and the
/// sessions are told — on a server that sleeps on `next_wake` between
/// passes, as the real one does (review 2026-10, P3-66).
#[test]
fn a_reloaded_config_rebuilds_the_channel_list() {
    let dir = temp_dir("config-reload");
    let path = dir.join("server.yaml");
    test_config(vec![
        open_channel("Lobby", true),
        open_channel("Ops", false),
    ])
    .save_to_file(&path)
    .expect("save");
    let mut w = World::new(Config::from_file(&path).expect("load"));
    w.server().watch_config_file(path.clone());
    let mut alice = w.connect_member("alice");
    alice.inbox.clear();

    let mut edited = Config::from_file(&path).expect("load");
    edited.channels.push(open_channel("Games", false));
    edited.save_to_file(&path).expect("save");
    // A coarse file clock must still see a new time.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now() + Duration::from_secs(5)))
        .expect("mtime");

    // Within a config check (3 s) and a margin, waking only when the
    // server asks to.
    let until = w.net.now() + Duration::from_secs(5);
    let mut names = None;
    while names.is_none() && w.net.now() < until {
        w.pump_on_wake(&mut [&mut alice]);
        names = alice.inbox.iter().find_map(|m| match m {
            ServerMessage::ChannelListUpdated { channels, .. } => {
                Some(channels.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
            }
            _ => None,
        });
    }
    let names = names.expect("the reload reached the session");
    assert!(names.iter().any(|n| n == "Games"), "{names:?}");
    assert!(names.iter().any(|n| n == "Ops"), "{names:?}");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn global_image_byte_budget_brakes_coordinated_bursts() {
    let mut config = test_config(vec![open_channel("Lobby", true)]);
    config.limits.max_transfer_kb_per_minute = 100_000; // loose
    config.limits.max_transfer_kb_per_minute_global = 100; // 100 KiB/min room-wide
    let mut w = World::new(config);
    let mut a = w.connect_member("alice");
    let mut b = w.connect_member("bob");

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
    config.limits.max_transfer_kb_per_minute = 100_000;
    config.limits.max_transfer_kb_per_minute_global = 100_000;
    config.limits.media_relay_kb_per_sec = 1024; // 1 MiB/s per receiver
    let mut w = World::new(config);
    let mut a = w.connect_member("alice");
    let mut b = w.connect_member("bob");

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

/// Voice the relay drops on purpose still arrived (review P2-2): a
/// muted stretch must not show up as loss in the sender's report once
/// the mute lifts.
#[test]
fn voice_dropped_for_a_mute_is_not_reported_as_lost() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    let alice_user = alice.key.user_id();

    let talk = |w: &mut World,
                alice: &mut TestClient,
                bob: &mut TestClient,
                admin: &mut TestClient,
                seqs: std::ops::Range<u32>| {
        for seq in seqs {
            let pkt = protocol::pack_voice(seq, seq * 20, &[0xAB; 60]).expect("voice");
            let now = w.net.now();
            w.net
                .driver(alice.node)
                .send(now, alice.peer, Lane::Voice, &pkt)
                .expect("send voice");
            w.settle(&mut [alice, bob, admin], Duration::from_millis(20));
        }
        w.settle(&mut [alice, bob, admin], Duration::from_millis(500));
        let mut got = 0;
        while w.net.driver(bob.node).recv(bob.peer, Lane::Voice).is_some() {
            got += 1;
        }
        got
    };
    assert_eq!(talk(&mut w, &mut alice, &mut bob, &mut admin, 0..20), 20);

    admin.admin(
        &mut w,
        1,
        AdminCommand::Restrict {
            target: AdminTarget::User(alice_user),
            kind: RestrictionKind::Voice,
            duration_secs: Some(30 * 60),
            reason: "hot mic".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 1),
        Ok(AdminOutcome::Done)
    );
    assert_eq!(talk(&mut w, &mut alice, &mut bob, &mut admin, 20..40), 0);

    w.advance_wall(Duration::from_mins(31));
    alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| {
        matches!(
            m,
            ServerMessage::RestrictionLifted {
                kind: RestrictionKind::Voice
            }
        )
        .then_some(())
    });
    assert_eq!(talk(&mut w, &mut alice, &mut bob, &mut admin, 40..60), 20);
    w.settle(
        &mut [&mut alice, &mut bob, &mut admin],
        Duration::from_secs(3),
    );

    let (mut received, mut expected, mut lost) = (0, 0, 0);
    for m in &alice.inbox {
        if let ServerMessage::VoiceReceiverReport {
            received: r,
            expected: e,
            lost: l,
            ..
        } = m
        {
            received += r;
            expected += e;
            lost += l;
        }
    }
    assert_eq!(
        (received, expected, lost),
        (60, 60, 0),
        "all sixty packets arrived, the muted twenty included"
    );
}

/// A session that sends no voice is sent no report of it — once it
/// has said it can do without (`QUIET_REPORTS`): a report of nothing
/// every two seconds kept a phone's radio up for as long as it sat in
/// the lobby. A client from before the bit reads a missing report as a
/// stalled link, and keeps getting them (review P2-49).
#[test]
fn a_silent_session_is_sent_no_voice_report_if_it_can_do_without() {
    let reports = |c: &TestClient| {
        c.inbox
            .iter()
            .filter(|m| matches!(m, ServerMessage::VoiceReceiverReport { .. }))
            .count()
    };
    let mut w = World::new(test_config(vec![
        open_channel("Lobby", true),
        open_channel("Room", false),
    ]));
    let mut quiet = w.connect("quiet");
    let mut older = w.open();
    older.features = features::CLIENT_1_1 & !features::QUIET_REPORTS;
    let code = w.guest_code.clone();
    w.admission(&mut older, "older", Auth::Invite { code })
        .expect("admitted");
    w.settle(&mut [&mut quiet, &mut older], Duration::from_secs(7));
    assert_eq!(reports(&quiet), 0, "nothing said, nothing reported");
    assert!(reports(&older) >= 3, "every two seconds, as it expects");

    // It talks: the reports come, and stop again after the last
    // interval that held any of its voice.
    quiet.join(&mut w, 1, None);
    for seq in 0..150u32 {
        let pkt = protocol::pack_voice(seq, seq * 960, &[0xAB; 60]).expect("voice");
        let now = w.net.now();
        w.net
            .driver(quiet.node)
            .send(now, quiet.peer, Lane::Voice, &pkt)
            .expect("send");
        w.settle(&mut [&mut quiet, &mut older], Duration::from_millis(20));
    }
    let while_talking = reports(&quiet);
    assert!(while_talking >= 1, "three seconds of voice, reported");
    w.settle(&mut [&mut quiet, &mut older], Duration::from_secs(3));
    let after = reports(&quiet);
    w.settle(&mut [&mut quiet, &mut older], Duration::from_secs(7));
    assert_eq!(reports(&quiet), after, "silent again, no more reports");
}

#[test]
fn video_groups_are_mirrored_to_subscribers_on_the_sharers_track() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut sharer = w.connect_member("sharer");
    let mut viewer = w.connect_member("viewer");
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

/// A viewer on a narrow link watches two streams: one busy, one that
/// has not sent a picture yet. Every group of the busy one is lost to
/// that viewer, and its sharer hears so in every report — a group is
/// two seconds long, so a one-second window read 0 % half the time —
/// while the quiet stream's sharer is not charged with losses that are
/// not its own (review P2-3).
#[test]
fn stream_health_is_per_stream_and_spans_whole_groups() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut busy = w.connect_member("busy");
    let mut quiet = w.connect_member("quiet");
    let mut viewer = w.connect_member("viewer");

    for sharer in [&mut busy, &mut quiet] {
        sharer.send(
            &mut w,
            &ClientMessage::ScreenShareStart {
                width: 640,
                height: 360,
                scope: ShareScope::Everyone,
            },
        );
    }
    for target_id in [busy.id, quiet.id] {
        viewer.wait_for(&mut w, &mut [&mut busy, &mut quiet], |m| {
            matches!(m, ServerMessage::ScreenShareStarted { client_id, .. } if *client_id == target_id)
                .then_some(())
        });
        viewer.send(&mut w, &ClientMessage::SubscribeScreen { target_id });
    }
    w.settle(
        &mut [&mut busy, &mut quiet, &mut viewer],
        Duration::from_millis(200),
    );
    busy.inbox.clear();
    quiet.inbox.clear();

    // The server reaches the viewer at 0.3 Mbit/s; the busy stream
    // offers well over a megabit.
    w.net.set_link_one_way(
        w.server,
        viewer.node,
        LinkSpec::clean(Duration::from_millis(20)).with_bandwidth(300_000, 100),
    );
    let start = w.net.now();
    let mut next_group = start;
    let mut next_frame = start;
    let mut seq = 0u32;
    let payload = vec![3u8; 20_000];
    while w.net.now() < start + Duration::from_secs(12) {
        let now = w.net.now();
        let drv = w.net.driver(busy.node);
        if now >= next_group {
            next_group = now + Duration::from_secs(2);
            drv.video_new_group(now, busy.peer, OWN_TRACK);
            next_frame = now;
        }
        if now >= next_frame {
            next_frame = now + Duration::from_millis(100);
            seq += 1;
            let frame = protocol::pack_video_frame(seq, false, 640, 360, seq * 100, &payload)
                .expect("frame");
            for chunk in &frame {
                let _ = drv.send(now, busy.peer, Lane::Video, chunk);
            }
        }
        w.pump(&mut [&mut busy, &mut quiet, &mut viewer]);
        while w.net.driver(viewer.node).recv_video(viewer.peer).is_some() {}
    }

    let health = |c: &TestClient| -> Vec<(f32, u16)> {
        c.inbox
            .iter()
            .filter_map(|m| match m {
                ServerMessage::StreamHealth {
                    worst_loss,
                    viewers,
                    ..
                } => Some((*worst_loss, *viewers)),
                _ => None,
            })
            .collect()
    };
    let busy_reports = health(&busy);
    let quiet_reports = health(&quiet);
    assert!(busy_reports.len() >= 10, "{busy_reports:?}");
    assert!(quiet_reports.len() >= 10, "{quiet_reports:?}");
    assert!(
        quiet_reports.iter().all(|r| *r == (0.0, 1)),
        "the quiet stream lost nothing: {quiet_reports:?}"
    );
    // The first group needs its half-second deadline to be given up
    // on; from then on no report reads clean.
    assert!(
        busy_reports[3..].iter().all(|(loss, _)| *loss > 0.0),
        "every later report shows the viewer behind: {busy_reports:?}"
    );
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
    // Let the admission rows land first, so the queue holds chats only
    // and no write is in flight when the disk stalls: the queue is
    // empty *and* the written count sat still for a moment.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut written_before = w.server().persistence().stats().written;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let stats = w.server().persistence().stats();
        if stats.queued == 0 && stats.written == written_before {
            break;
        }
        written_before = stats.written;
        assert!(
            Instant::now() < deadline,
            "admission rows never landed: {stats}"
        );
    }

    // The disk stalls. The thread is real; give it a moment to park.
    let stalled = w.server().persistence().stall();
    std::thread::sleep(Duration::from_millis(50));
    let at_stall = w.server().persistence().stats();

    // 20 chats: what the queue has room for is queued, the rest is
    // dropped and counted — every one of them still delivered. (Other
    // writes may ride along; the chats' own share is the delta.)
    for i in 0..20 {
        a.send(&mut w, &ClientMessage::Chat(format!("m{i}")));
    }
    for i in 0..20 {
        let wanted = format!("m{i}");
        b.wait_for(&mut w, &mut [&mut a], |m| {
            matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == wanted).then_some(())
        });
    }
    let stats = w.server().persistence().stats();
    let attempted = u64::try_from(stats.queued - at_stall.queued).unwrap_or(0)
        + (stats.dropped_writes - at_stall.dropped_writes);
    assert!(attempted >= 20, "{stats}");
    assert_eq!(stats.queued, 8, "the queue is full: {stats}");
    assert!(stats.dropped_writes >= 12, "{stats}");
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

    // The disk comes back: what was queued lands, what was dropped is
    // gone for good.
    let queued = u64::try_from(w.server().persistence().stats().queued).unwrap_or(0);
    drop(stalled);
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().written < written_before + queued {
        assert!(
            Instant::now() < deadline,
            "queued writes never landed: {}",
            w.server().persistence().stats()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(w);
    let stored = stored_message_count(&dir);
    assert!((1..=8).contains(&stored), "{stored} messages stored");
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
                ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. }
                    if c.text == format!("first run {i}") =>
                {
                    Some(c.id)
                }
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
        ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. }
            if c.text == "second run" =>
        {
            Some(c.id)
        }
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
        matches!(m, ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "hello").then_some(())
    });
    let stats = w.server().persistence().stats();
    assert!(!stats.persistent);
    assert_eq!(stats.dropped_writes, 0);
    drop(w);
    assert!(
        !dir.join("baucord.db").exists(),
        "memory mode must not create a database"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Profiles and avatars (spec §7, §7.1) ─────────────────────────────

fn set_profile(
    client: &mut TestClient,
    w: &mut World,
    color: Option<u32>,
    avatar: Option<[u8; 32]>,
) {
    // `wait_for` does not consume: an earlier verdict must not be
    // mistaken for this attempt's.
    let me = client.key.user_id();
    client.inbox.retain(|m| {
        !matches!(m, ServerMessage::ProfileDenied { .. })
            && !matches!(m, ServerMessage::ProfileUpdated { user_id, .. } if *user_id == me)
    });
    client.send(
        w,
        &ClientMessage::SetProfile(protocol::Profile {
            name_color: color,
            name_style: protocol::NameStyle::Bold,
            avatar,
        }),
    );
}

fn profile_verdict(
    client: &mut TestClient,
    w: &mut World,
    others: &mut [&mut TestClient],
) -> Result<protocol::Profile, protocol::ProfileDenyReason> {
    let me = client.key.user_id();
    client.wait_for(w, others, |m| match m {
        ServerMessage::ProfileUpdated { user_id, profile } if *user_id == me => {
            Some(Ok(profile.clone()))
        }
        ServerMessage::ProfileDenied { reason } => Some(Err(*reason)),
        _ => None,
    })
}

#[test]
fn profiles_reach_everyone_and_late_joiners() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    set_profile(&mut alice, &mut w, Some(0x00E0_6C75), None);
    let seen = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::ProfileUpdated { user_id, profile } if *user_id == alice_user => {
            Some(profile.clone())
        }
        _ => None,
    });
    assert_eq!(seen.name_color, Some(0x00E0_6C75));
    assert_eq!(seen.name_style, protocol::NameStyle::Bold);
    assert!(profile_verdict(&mut alice, &mut w, &mut [&mut bob]).is_ok());

    // A late joiner learns everyone's look with the roster.
    let mut carol = w.connect("carol");
    let alice_id = alice.id;
    let at_join = carol.wait_for(&mut w, &mut [&mut alice, &mut bob], |m| match m {
        ServerMessage::UserProfile { client_id, profile } if *client_id == alice_id => {
            Some(profile.clone())
        }
        _ => None,
    });
    assert_eq!(at_join.name_color, Some(0x00E0_6C75));
}

#[test]
fn the_gold_band_and_low_contrast_are_the_servers_call() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    set_profile(&mut alice, &mut w, Some(0x00FF_D700), None);
    assert_eq!(
        profile_verdict(&mut alice, &mut w, &mut [&mut admin]),
        Err(protocol::ProfileDenyReason::ReservedColor)
    );
    set_profile(&mut alice, &mut w, Some(0x0020_2020), None);
    assert_eq!(
        profile_verdict(&mut alice, &mut w, &mut [&mut admin]),
        Err(protocol::ProfileDenyReason::LowContrast)
    );
    // Refusals do not count against the rate limit: a good color lands.
    set_profile(&mut alice, &mut w, Some(0x004A_9AE0), None);
    assert!(profile_verdict(&mut alice, &mut w, &mut [&mut admin]).is_ok());
    // Gold is an admin's to take.
    set_profile(&mut admin, &mut w, Some(0x00FF_D700), None);
    let gold = profile_verdict(&mut admin, &mut w, &mut [&mut alice]).expect("admin gold");
    assert_eq!(gold.name_color, Some(0x00FF_D700));
}

#[test]
fn gold_is_a_permission_of_its_own() {
    use server::persistence::{cli_role_create, cli_set_roles};
    let dir = temp_dir("gold-permission");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let alice_user = alice.key.user_id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let db = dir.join("baucord.db");
    // "Everything but gold": an administrator's powers without the band.
    let everything = permissions::CUSTOM_ROLE_PERMISSIONS & !permissions::GOLD_NAME;
    assert!(permissions::has(everything, permissions::BAN));
    let id = cli_role_create(&db, "steward", everything, "", 0).expect("create");
    cli_set_roles(&db, &alice_user, &[id]).expect("grant");
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut alice, &mut admin]);
        if alice.inbox.iter().any(|m| {
            matches!(m, ServerMessage::Session { permissions, .. } if *permissions == everything)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the role never reached the session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // She may ban, but not wear gold.
    set_profile(&mut alice, &mut w, Some(0x00FF_D700), None);
    assert_eq!(
        profile_verdict(&mut alice, &mut w, &mut [&mut admin]),
        Err(protocol::ProfileDenyReason::ReservedColor)
    );
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profile_changes_are_rate_limited() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    set_profile(&mut alice, &mut w, Some(0x00E0_6C75), None);
    assert!(profile_verdict(&mut alice, &mut w, &mut []).is_ok());
    set_profile(&mut alice, &mut w, Some(0x004A_9AE0), None);
    assert_eq!(
        profile_verdict(&mut alice, &mut w, &mut []),
        Err(protocol::ProfileDenyReason::RateLimited)
    );
    w.settle(&mut [&mut alice], Duration::from_secs(31));
    set_profile(&mut alice, &mut w, Some(0x004A_9AE0), None);
    assert!(profile_verdict(&mut alice, &mut w, &mut []).is_ok());
}

#[test]
fn an_avatar_uploads_once_and_reaches_everyone() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    let picture = png_blob(3, 8 * 1024);

    // Upload, then point the profile at the hash.
    let blob_id = alice.upload_blob(&mut w, 1, "image/png", &picture);
    let done = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UploadDone {
            upload_id: 1,
            blob_id,
        } => Some(*blob_id),
        ServerMessage::UploadRejected { reason, .. } => panic!("rejected: {reason:?}"),
        _ => None,
    });
    assert_eq!(done, blob_id);
    set_profile(&mut alice, &mut w, None, Some(blob_id));
    let seen = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::ProfileUpdated { user_id, profile } if *user_id == alice_user => {
            Some(profile.clone())
        }
        _ => None,
    });
    assert_eq!(seen.avatar, Some(blob_id));

    // Bob fetches it and gets the very bytes back.
    bob.send(&mut w, &ClientMessage::FetchBlob { blob_id });
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(2));
    assert_eq!(
        bob.blob_received(&blob_id).as_deref(),
        Some(picture.as_slice())
    );

    // The same bytes again: acknowledged, never transferred.
    alice.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 2,
            name: "again.png".into(),
            mime: "image/png".into(),
            size: u32::try_from(picture.len()).unwrap(),
            sha256: blob_id,
        },
    );
    let needed = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UploadAccept {
            upload_id: 2,
            needed,
        } => Some(*needed),
        _ => None,
    });
    assert!(!needed, "a known hash needs no transfer");

    // Over the avatar cap: refused at the announcement.
    let huge = png_blob(4, 300 * 1024);
    alice.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 3,
            name: "huge.png".into(),
            mime: "image/png".into(),
            size: u32::try_from(huge.len()).unwrap(),
            sha256: transport::sha256::digest(&huge),
        },
    );
    let why = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UploadRejected {
            upload_id: 3,
            reason,
        } => Some(*reason),
        _ => None,
    });
    assert_eq!(why, protocol::UploadRejectReason::TooLarge);

    // A hash that does not match the bytes: refused once whole.
    let other = png_blob(5, 4 * 1024);
    alice.send(
        &mut w,
        &ClientMessage::UploadBegin {
            upload_id: 4,
            name: "liar.png".into(),
            mime: "image/png".into(),
            size: u32::try_from(other.len()).unwrap(),
            sha256: [9u8; 32],
        },
    );
    alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UploadAccept { upload_id: 4, .. } => Some(()),
        _ => None,
    });
    alice.send_blob_chunks(&mut w, 4, &other);
    let why = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UploadRejected {
            upload_id: 4,
            reason,
        } => Some(*reason),
        _ => None,
    });
    assert_eq!(why, protocol::UploadRejectReason::HashMismatch);

    // A profile pointing at a hash the server never saw.
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(31));
    set_profile(&mut alice, &mut w, None, Some([9u8; 32]));
    assert_eq!(
        profile_verdict(&mut alice, &mut w, &mut [&mut bob]),
        Err(protocol::ProfileDenyReason::BadAvatar)
    );
}

#[test]
fn an_admin_resets_and_locks_a_profile() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let alice_user = alice.key.user_id();
    set_profile(&mut alice, &mut w, Some(0x00E0_6C75), None);
    assert!(profile_verdict(&mut alice, &mut w, &mut [&mut admin]).is_ok());

    alice.inbox.clear();
    admin.admin(
        &mut w,
        1,
        AdminCommand::ResetProfile {
            target: AdminTarget::User(alice_user),
            lock: true,
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 1),
        Ok(AdminOutcome::Done)
    );
    let cleared = alice.wait_for(&mut w, &mut [&mut admin], |m| match m {
        ServerMessage::ProfileUpdated { user_id, profile } if *user_id == alice_user => {
            Some(profile.clone())
        }
        _ => None,
    });
    assert_eq!(cleared, protocol::Profile::default());
    alice.wait_for(&mut w, &mut [&mut admin], |m| {
        matches!(
            m,
            ServerMessage::Restricted {
                kind: RestrictionKind::Profile,
                ..
            }
        )
        .then_some(())
    });
    set_profile(&mut alice, &mut w, Some(0x004A_9AE0), None);
    assert_eq!(
        profile_verdict(&mut alice, &mut w, &mut [&mut admin]),
        Err(protocol::ProfileDenyReason::Locked)
    );

    // Lifted like any restriction; the rate limit still counts from
    // the last change that landed.
    admin.admin(
        &mut w,
        2,
        AdminCommand::Unmute {
            target: AdminTarget::User(alice_user),
            kind: RestrictionKind::Profile,
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice], 2),
        Ok(AdminOutcome::Done)
    );
    w.settle(&mut [&mut alice, &mut admin], Duration::from_secs(31));
    set_profile(&mut alice, &mut w, Some(0x004A_9AE0), None);
    assert!(profile_verdict(&mut alice, &mut w, &mut [&mut admin]).is_ok());
}

#[test]
fn a_rename_reaches_every_roster_and_keeps_names_unique() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_id = alice.id;
    alice.send(
        &mut w,
        &ClientMessage::SetNickname {
            name: "alicia".into(),
        },
    );
    let renamed = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserRenamed { client_id, name } if *client_id == alice_id => {
            Some(name.clone())
        }
        _ => None,
    });
    assert_eq!(renamed, "alicia");
    assert_eq!(
        w.server().policy().users[&alice.key.user_id()].name,
        "alicia"
    );

    // Bob's name, in any case, is taken; one rename per 30 s. Earlier
    // verdicts are cleared before each attempt (`wait_for` does not
    // consume).
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(31));
    alice
        .inbox
        .retain(|m| !matches!(m, ServerMessage::RenameDenied { .. }));
    alice.send(&mut w, &ClientMessage::SetNickname { name: "BOB".into() });
    let why = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::RenameDenied { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(why, protocol::RenameDenyReason::Taken);
    alice.send(&mut w, &ClientMessage::SetNickname { name: "ali".into() });
    alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::UserRenamed { client_id, name }
            if *client_id == alice_id && name == "ali" =>
        {
            Some(())
        }
        _ => None,
    });
    alice
        .inbox
        .retain(|m| !matches!(m, ServerMessage::RenameDenied { .. }));
    alice.send(&mut w, &ClientMessage::SetNickname { name: "al".into() });
    let why = alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::RenameDenied { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(why, protocol::RenameDenyReason::RateLimited);
}

#[test]
fn profiles_and_avatars_survive_a_restart() {
    let dir = temp_dir("profiles");
    let key = Arc::new(ClientKey::generate());
    let alice_user = key.user_id();
    let picture = png_blob(6, 6 * 1024);
    let blob_id = transport::sha256::digest(&picture);
    {
        let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
        let code = w.invite(ROLE_MEMBER);
        let mut alice = w.connect_with_key("alice", Auth::Invite { code }, Arc::clone(&key));
        let uploaded = alice.upload_blob(&mut w, 1, "image/png", &picture);
        assert_eq!(uploaded, blob_id);
        alice.wait_for(&mut w, &mut [], |m| match m {
            ServerMessage::UploadDone { upload_id: 1, .. } => Some(()),
            _ => None,
        });
        set_profile(&mut alice, &mut w, Some(0x00E0_6C75), Some(blob_id));
        assert!(profile_verdict(&mut alice, &mut w, &mut []).is_ok());
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.server().persistence().stats().queued > 0 {
            assert!(Instant::now() < deadline, "writes never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let stored = &w.server().policy().users[&alice_user].profile;
    assert_eq!(stored.name_color, Some(0x00E0_6C75));
    assert_eq!(stored.avatar, Some(blob_id));
    assert!(w.server().policy().blobs.contains_key(&blob_id));

    // Bob is here first; alice's look arrives with her roster line,
    // and her picture comes back from disk.
    let mut bob = w.connect_member("bob");
    let mut alice = w.connect_with_key("alice", Auth::Key, key);
    let alice_id = alice.id;
    let seen = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::UserProfile { client_id, profile } if *client_id == alice_id => {
            Some(profile.clone())
        }
        _ => None,
    });
    assert_eq!(seen.avatar, Some(blob_id));
    bob.send(&mut w, &ClientMessage::FetchBlob { blob_id });
    let deadline = Instant::now() + Duration::from_secs(5);
    while bob.blob_received(&blob_id).is_none() {
        assert!(Instant::now() < deadline, "blob never came back from disk");
        std::thread::sleep(Duration::from_millis(10));
        w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(100));
    }
    assert_eq!(
        bob.blob_received(&blob_id).as_deref(),
        Some(picture.as_slice())
    );
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Direct messages (spec §6) ────────────────────────────────────────

fn dm(client: &mut TestClient, w: &mut World, to: UserId, text: &str, client_ref: u32) {
    client.send(
        w,
        &ClientMessage::DirectMessage {
            to,
            text: text.to_string(),
            client_ref,
        },
    );
}

/// The sender's verdict for `client_ref`: the message id, or why not.
fn dm_verdict(
    client: &mut TestClient,
    w: &mut World,
    others: &mut [&mut TestClient],
    client_ref: u32,
) -> Result<u64, protocol::DmFailReason> {
    client.wait_for(w, others, |m| match m {
        ServerMessage::MessageAck {
            client_ref: r,
            msg_id,
        } if *r == client_ref => Some(Ok(*msg_id)),
        ServerMessage::MessageFailed {
            client_ref: r,
            reason,
        } if *r == client_ref => Some(Err(*reason)),
        _ => None,
    })
}

#[test]
fn members_dm_each_other_live_and_guests_are_refused() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let mut carol = w.connect("carol");
    let alice_user = alice.key.user_id();
    let bob_user = bob.key.user_id();
    let carol_user = carol.key.user_id();

    dm(&mut alice, &mut w, bob_user, "psst", 7);
    let got = bob.wait_for(&mut w, &mut [&mut alice, &mut carol], |m| match m {
        ServerMessage::Direct { from, msg } if *from == alice_user => Some(msg.clone()),
        _ => None,
    });
    assert_eq!(got.text, "psst");
    assert_eq!(got.client_id, alice.id);
    let id = dm_verdict(&mut alice, &mut w, &mut [&mut bob, &mut carol], 7).expect("acked");
    assert_eq!(id, got.id);
    // Nobody else saw it.
    assert!(!carol
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Direct { .. })));

    // Guests neither send nor receive.
    dm(&mut carol, &mut w, alice_user, "hi?", 1);
    assert_eq!(
        dm_verdict(&mut carol, &mut w, &mut [&mut alice, &mut bob], 1),
        Err(protocol::DmFailReason::NoPermission)
    );
    dm(&mut alice, &mut w, carol_user, "hi!", 8);
    assert_eq!(
        dm_verdict(&mut alice, &mut w, &mut [&mut bob, &mut carol], 8),
        Err(protocol::DmFailReason::NoPermission)
    );
}

#[test]
fn a_dm_to_someone_offline_fails_without_a_store() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let bob = w.connect_member("bob");
    let bob_user = bob.key.user_id();
    bob.leave(&mut w);
    w.settle(&mut [&mut alice], Duration::from_secs(1));
    dm(&mut alice, &mut w, bob_user, "anyone?", 3);
    assert_eq!(
        dm_verdict(&mut alice, &mut w, &mut [], 3),
        Err(protocol::DmFailReason::Offline)
    );
}

#[test]
fn a_dm_restriction_refuses_the_sender() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    admin.admin(
        &mut w,
        1,
        AdminCommand::Restrict {
            target: AdminTarget::User(alice.key.user_id()),
            kind: RestrictionKind::Dm,
            duration_secs: None,
            reason: "no dms".to_string(),
        },
    );
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 1),
        Ok(AdminOutcome::Done)
    );
    dm(&mut alice, &mut w, bob.key.user_id(), "still?", 2);
    assert_eq!(
        dm_verdict(&mut alice, &mut w, &mut [&mut admin, &mut bob], 2),
        Err(protocol::DmFailReason::NoPermission)
    );
    assert!(alice.inbox.iter().any(|m| matches!(
        m,
        ServerMessage::Restricted {
            kind: RestrictionKind::Dm,
            ..
        }
    )));
}

#[test]
fn dms_are_stored_first_wait_for_the_offline_and_count_unread() {
    let dir = temp_dir("dms");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let bob_key = Arc::new(ClientKey::generate());
    let code = w.invite(ROLE_MEMBER);
    let mut bob = w.connect_with_key("bob", Auth::Invite { code }, Arc::clone(&bob_key));
    let alice_user = alice.key.user_id();
    let bob_user = bob_key.user_id();

    // Online: stored, then acked and delivered in the same tick.
    dm(&mut alice, &mut w, bob_user, "one", 1);
    let first = dm_verdict(&mut alice, &mut w, &mut [&mut bob], 1).expect("stored");
    bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::Direct { from, msg } if *from == alice_user && msg.id == first => Some(()),
        _ => None,
    });

    // Offline: stored and acked; nobody to deliver to right now.
    bob.leave(&mut w);
    w.settle(&mut [&mut alice], Duration::from_secs(1));
    dm(&mut alice, &mut w, bob_user, "two", 2);
    let second = dm_verdict(&mut alice, &mut w, &mut [], 2).expect("stored while offline");
    assert!(second > first);
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "writes never landed");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Back: the conversation list says two unread from alice, the
    // page has both, and a read mark clears the count.
    let mut bob = w.connect_with_key("bob", Auth::Key, Arc::clone(&bob_key));
    let list = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::Conversations { dms } => Some(dms.clone()),
        _ => None,
    });
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].user_id, alice_user);
    assert_eq!(list[0].name, "alice");
    assert_eq!(list[0].unread, 2);
    assert_eq!(list[0].last_msg_id, second);
    bob.send(
        &mut w,
        &ClientMessage::FetchHistory {
            conv: ConvRef::Dm(alice_user),
            before: None,
            limit: 50,
        },
    );
    let page = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::History {
            conv: ConvRef::Dm(other),
            messages,
            ..
        } if *other == alice_user => Some(messages.clone()),
        _ => None,
    });
    let texts: Vec<&str> = page.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
    bob.send(
        &mut w,
        &ClientMessage::MarkRead {
            conv: ConvRef::Dm(alice_user),
            msg_id: second,
        },
    );
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "writes never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    bob.leave(&mut w);
    let mut bob = w.connect_with_key("bob", Auth::Key, bob_key);
    let list = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::Conversations { dms } => Some(dms.clone()),
        _ => None,
    });
    assert_eq!(list[0].unread, 0);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stalled_store_fails_a_dm_within_the_deadline() {
    let dir = temp_dir("dm-stall");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let stalled = w.server().persistence().stall();
    std::thread::sleep(Duration::from_millis(50));
    dm(&mut alice, &mut w, bob.key.user_id(), "hello?", 4);
    assert_eq!(
        dm_verdict(&mut alice, &mut w, &mut [&mut bob], 4),
        Err(protocol::DmFailReason::Store)
    );
    // Nothing was delivered: stored first, or not at all.
    assert!(!bob
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Direct { .. })));
    drop(stalled);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file sent as a DM takes a reference when it is sent. A store that
/// does not answer fails the DM — and the reference goes back: it used
/// to stay, and a file with a count above zero is never deleted
/// (review P2-11).
#[test]
fn a_file_dm_that_was_not_stored_gives_its_reference_back_once_the_store_says_so() {
    let dir = temp_dir("dm-file-stall");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let bob_user = bob.key.user_id();
    let (_, blob) = post_file(
        &mut alice,
        &mut w,
        &mut [&mut bob],
        30,
        "notes.txt",
        "text/plain",
        b"the same file, twice",
        ConvRef::General,
    )
    .expect("posted in the room");
    let refs = |w: &mut World| w.server().policy().blobs.get(&blob).map(|b| b.refs);
    assert_eq!(refs(&mut w), Some(1));

    let send_as_dm = |alice: &mut TestClient, w: &mut World, bob: &mut TestClient, client_ref| {
        alice.send(
            w,
            &ClientMessage::SendFile {
                conv: ConvRef::Dm(bob_user),
                blob_id: blob,
                client_ref,
            },
        );
        alice.wait_for(w, &mut [bob], |m| match m {
            ServerMessage::MessageAck { client_ref: r, .. } if *r == client_ref => Some(Ok(())),
            ServerMessage::MessageFailed {
                client_ref: r,
                reason,
            } if *r == client_ref => Some(Err(*reason)),
            _ => None,
        })
    };

    let stalled = w.server().persistence().stall();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        send_as_dm(&mut alice, &mut w, &mut bob, 71),
        Err(DmFailReason::Store)
    );
    // A timeout does not say whether it was stored: the reference stays
    // until the store says what points at the file (review 2026-10,
    // P2-24) — given back at once, a DM written after all could lose
    // its file to the prune.
    assert_eq!(refs(&mut w), Some(2), "kept while unknown");
    drop(stalled);
    let deadline = Instant::now() + Duration::from_secs(10);
    while refs(&mut w) != Some(1) {
        assert!(
            Instant::now() < deadline,
            "the recount never came: {:?}",
            refs(&mut w)
        );
        w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(send_as_dm(&mut alice, &mut w, &mut bob, 72), Ok(()));
    assert_eq!(refs(&mut w), Some(2), "the room's post and the DM");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Block, report, delete my account (1.2, protocol::safety) ────────────

/// The block list the server last sent this client.
fn last_blocks(client: &TestClient) -> Option<Vec<UserId>> {
    client.inbox.iter().rev().find_map(|m| match m {
        ServerMessage::Blocks { users } => Some(users.iter().map(|u| u.user_id).collect()),
        _ => None,
    })
}

/// Send `Block`/`Unblock` and wait for the list that answers it.
fn set_block(
    client: &mut TestClient,
    w: &mut World,
    others: &mut [&mut TestClient],
    user: UserId,
    block: bool,
) -> Vec<protocol::BlockedUser> {
    client
        .inbox
        .retain(|m| !matches!(m, ServerMessage::Blocks { .. }));
    client.send(
        w,
        &if block {
            ClientMessage::Block { user }
        } else {
            ClientMessage::Unblock { user }
        },
    );
    client.wait_for(w, others, |m| match m {
        ServerMessage::Blocks { users } => Some(users.clone()),
        _ => None,
    })
}

/// Send `Report` and wait for its answer.
fn report(
    client: &mut TestClient,
    w: &mut World,
    others: &mut [&mut TestClient],
    target: protocol::ReportTarget,
    reason: protocol::ReportReason,
) -> Result<(), protocol::ReportRefusal> {
    client
        .inbox
        .retain(|m| !matches!(m, ServerMessage::ReportAck { .. }));
    client.send(
        w,
        &ClientMessage::Report {
            target,
            reason,
            note: "seen it twice".to_string(),
        },
    );
    client.wait_for(w, others, |m| match m {
        ServerMessage::ReportAck { result } => Some(*result),
        _ => None,
    })
}

/// `stored_rows`, once the persistence thread (real time, not the
/// simulated clock) has caught up to `want`; the count either way.
fn rows_become(dir: &std::path::Path, table: &str, filter: &str, user: &UserId, want: i64) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = stored_rows(dir, table, filter, user);
        if n == want || Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Count rows in `table` of the database at `dir` matching `filter`.
fn stored_rows(dir: &std::path::Path, table: &str, filter: &str, user: &UserId) -> i64 {
    let conn = rusqlite::Connection::open(dir.join("baucord.db")).expect("open db");
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {filter}"),
        rusqlite::params![user.as_slice()],
        |r| r.get(0),
    )
    .expect("count")
}

#[test]
fn a_block_is_sent_on_join_refuses_dms_both_ways_and_is_undone() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let bob_user = bob.key.user_id();
    let alice_user = alice.key.user_id();
    // On join: an empty list, before anything else needs it.
    assert_eq!(last_blocks(&alice), Some(Vec::new()));

    let list = set_block(&mut alice, &mut w, &mut [&mut bob], bob_user, true);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].user_id, bob_user);
    assert_eq!(list[0].name, "bob");
    // Bob is not told.
    assert!(!bob
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Blocks { users } if !users.is_empty())));

    // Neither can write to the other, said the same neutral way.
    dm(&mut bob, &mut w, alice_user, "hey", 1);
    assert_eq!(
        dm_verdict(&mut bob, &mut w, &mut [&mut alice], 1),
        Err(DmFailReason::Unavailable)
    );
    dm(&mut alice, &mut w, bob_user, "go away", 2);
    assert_eq!(
        dm_verdict(&mut alice, &mut w, &mut [&mut bob], 2),
        Err(DmFailReason::Unavailable)
    );
    assert!(!alice
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::Direct { .. })));

    // Blocking yourself or nobody changes nothing.
    assert_eq!(
        set_block(&mut alice, &mut w, &mut [&mut bob], alice_user, true).len(),
        1
    );
    assert_eq!(
        set_block(&mut alice, &mut w, &mut [&mut bob], [9u8; 32], true).len(),
        1
    );

    assert!(set_block(&mut alice, &mut w, &mut [&mut bob], bob_user, false).is_empty());
    dm(&mut bob, &mut w, alice_user, "sorry", 3);
    assert!(dm_verdict(&mut bob, &mut w, &mut [&mut alice], 3).is_ok());
}

#[test]
fn a_blocked_persons_posts_and_edits_never_reach_the_blocker() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let mut carol = w.connect_member("carol");
    let bob_user = bob.key.user_id();
    set_block(
        &mut alice,
        &mut w,
        &mut [&mut bob, &mut carol],
        bob_user,
        true,
    );

    bob.send(&mut w, &ClientMessage::Chat("hey alice".to_string()));
    // Carol gets it with Bob's account; Alice gets nothing.
    let (id, from) = carol.wait_for(&mut w, &mut [&mut alice, &mut bob], |m| match m {
        ServerMessage::ChatFrom { from, msg } if msg.text == "hey alice" => Some((msg.id, *from)),
        _ => None,
    });
    assert_eq!(from, bob_user);
    bob.send(
        &mut w,
        &ClientMessage::EditMessage {
            msg_id: id,
            text: "hey everyone".to_string(),
        },
    );
    carol.wait_for(&mut w, &mut [&mut alice, &mut bob], |m| match m {
        ServerMessage::MessageEdited { msg_id, .. } if *msg_id == id => Some(()),
        _ => None,
    });
    w.settle(
        &mut [&mut alice, &mut bob, &mut carol],
        Duration::from_millis(200),
    );
    assert!(
        !alice.inbox.iter().any(|m| matches!(m,
            ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.id == id)
            || matches!(m, ServerMessage::MessageEdited { msg_id, .. } if *msg_id == id)),
        "the blocker saw the post or its edit"
    );
    // Unblocked: the next one arrives.
    set_block(
        &mut alice,
        &mut w,
        &mut [&mut bob, &mut carol],
        bob_user,
        false,
    );
    bob.send(&mut w, &ClientMessage::Chat("again".to_string()));
    alice.wait_for(&mut w, &mut [&mut bob, &mut carol], |m| match m {
        ServerMessage::ChatFrom { msg, .. } if msg.text == "again" => Some(()),
        _ => None,
    });
}

/// Who sent it (1.2): a post from someone in the lobby, whose join
/// nobody saw, comes after an `Authors` naming them, their session and
/// their look; a room member's post needs none. A history page comes
/// after the names and looks of its authors, online or not.
#[test]
fn a_post_from_the_lobby_and_a_history_page_name_their_authors() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_lobby("bob");
    let (bob_id, bob_user) = (bob.id, bob.key.user_id());
    let alice_user = alice.key.user_id();
    set_profile(&mut bob, &mut w, Some(0x0088_CCFF), None);
    profile_verdict(&mut bob, &mut w, &mut [&mut alice]).expect("a readable color");

    bob.send(
        &mut w,
        &ClientMessage::Chat("hi from the lobby".to_string()),
    );
    alice.wait_for(&mut w, &mut [&mut bob], |m| match m {
        ServerMessage::ChatFrom { msg, .. } if msg.text == "hi from the lobby" => Some(()),
        _ => None,
    });
    let intro = alice
        .inbox
        .iter()
        .position(|m| {
            matches!(m, ServerMessage::Authors { authors }
            if authors.iter().any(|a| a.user_id == bob_user))
        })
        .expect("bob introduced");
    let post = alice
        .inbox
        .iter()
        .position(
            |m| matches!(m, ServerMessage::ChatFrom { msg, .. } if msg.text == "hi from the lobby"),
        )
        .expect("the post");
    assert!(intro < post, "the name comes before the post");
    let ServerMessage::Authors { authors } = &alice.inbox[intro] else {
        unreachable!()
    };
    assert_eq!(authors.len(), 1);
    assert_eq!(authors[0].client_id, Some(bob_id));
    assert_eq!(authors[0].name, "bob");
    assert_eq!(authors[0].profile.name_color, Some(0x0088_CCFF));

    // Alice is in a room: bob was told about her with the join.
    alice.send(&mut w, &ClientMessage::Chat("hello lobby".to_string()));
    bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::ChatFrom { msg, .. } if msg.text == "hello lobby" => Some(()),
        _ => None,
    });
    assert!(
        !bob.inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::Authors { authors }
            if authors.iter().any(|a| a.user_id == alice_user))),
        "a room member needs no introduction"
    );

    // Bob leaves; carol arrives later and reads both posts back.
    bob.leave(&mut w);
    let carol = w.connect_member("carol");
    let carol_user = carol.key.user_id();
    let page = carol
        .inbox
        .iter()
        .position(|m| matches!(m, ServerMessage::History { .. }))
        .expect("the replay");
    let (at, authors) = carol
        .inbox
        .iter()
        .enumerate()
        .find_map(|(i, m)| match m {
            ServerMessage::Authors { authors } => Some((i, authors.clone())),
            _ => None,
        })
        .expect("the page's authors");
    assert!(at < page, "the authors come before the page");
    let bob_look = authors
        .iter()
        .find(|a| a.user_id == bob_user)
        .expect("bob, offline, is named");
    assert_eq!(bob_look.client_id, None);
    assert_eq!(bob_look.name, "bob");
    assert_eq!(bob_look.profile.name_color, Some(0x0088_CCFF));
    assert!(authors.iter().any(|a| a.user_id == alice_user));
    assert!(
        !authors.iter().any(|a| a.user_id == carol_user),
        "never the reader"
    );
}

#[test]
fn an_older_client_hears_no_permission_instead_of_unavailable() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let alice_user = alice.key.user_id();
    // Bob runs a 1.1 client: no `SAFETY`, so no `Unavailable` either.
    let mut bob = w.open();
    bob.features = features::CLIENT_1_1 & !features::SAFETY;
    let code = w.member_code.clone();
    bob.hello(&mut w, "bob", Auth::Invite { code });
    bob.wait_for(&mut w, &mut [&mut alice], |m| {
        matches!(m, ServerMessage::Init { .. }).then_some(())
    });
    assert_eq!(last_blocks(&bob), None, "never sent without the bit");
    let bob_user = bob.key.user_id();
    set_block(&mut alice, &mut w, &mut [&mut bob], bob_user, true);
    dm(&mut bob, &mut w, alice_user, "hey", 1);
    assert_eq!(
        dm_verdict(&mut bob, &mut w, &mut [&mut alice], 1),
        Err(DmFailReason::NoPermission)
    );
}

#[test]
fn a_block_keeps_the_blocked_voice_from_the_blocker_only() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let mut carol = w.connect_member("carol");
    let bob_user = bob.key.user_id();

    // `from` speaks 10 packets; how many reach `to`.
    let voice = |w: &mut World,
                 from: &mut TestClient,
                 to: &mut TestClient,
                 third: &mut TestClient,
                 seq0: u32| {
        for seq in seq0..seq0 + 10 {
            let pkt = protocol::pack_voice(seq, seq * 20, &[0xAB; 60]).expect("voice");
            let now = w.net.now();
            w.net
                .driver(from.node)
                .send(now, from.peer, Lane::Voice, &pkt)
                .expect("send voice");
            w.settle(&mut [from, to, third], Duration::from_millis(20));
        }
        w.settle(&mut [from, to, third], Duration::from_millis(500));
        let mut got = 0;
        while w.net.driver(to.node).recv(to.peer, Lane::Voice).is_some() {
            got += 1;
        }
        while w
            .net
            .driver(third.node)
            .recv(third.peer, Lane::Voice)
            .is_some()
        {}
        got
    };
    assert_eq!(voice(&mut w, &mut bob, &mut alice, &mut carol, 0), 10);

    set_block(
        &mut alice,
        &mut w,
        &mut [&mut bob, &mut carol],
        bob_user,
        true,
    );
    assert_eq!(voice(&mut w, &mut bob, &mut alice, &mut carol, 100), 0);
    // Carol still hears Bob; Bob still hears Alice.
    assert_eq!(voice(&mut w, &mut bob, &mut carol, &mut alice, 200), 10);
    assert_eq!(voice(&mut w, &mut alice, &mut bob, &mut carol, 300), 10);

    set_block(
        &mut alice,
        &mut w,
        &mut [&mut bob, &mut carol],
        bob_user,
        false,
    );
    assert_eq!(voice(&mut w, &mut bob, &mut alice, &mut carol, 400), 10);
}

#[test]
fn blocks_are_stored_and_come_back_after_a_restart() {
    let dir = temp_dir("blocks");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    let bob_user = bob.key.user_id();
    set_block(&mut alice, &mut w, &mut [&mut bob], bob_user, true);
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));
    assert_eq!(
        rows_become(&dir, "blocks", "blocker = ?1", &alice_user, 1),
        1
    );
    // A restart reads it back with the policy.
    drop(alice);
    drop(bob);
    drop(w);
    let mut again = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    assert!(again
        .server()
        .policy()
        .blocks
        .get(&alice_user)
        .is_some_and(|s| s.contains(&bob_user)));
}

#[test]
fn a_report_reaches_the_moderators_and_waits_in_the_reports_tab() {
    use protocol::{ReportReason, ReportRefusal, ReportTarget};
    let dir = temp_dir("reports");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    let bob_user = bob.key.user_id();

    bob.send(
        &mut w,
        &ClientMessage::Chat("you are all idiots".to_string()),
    );
    let msg_id = alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| match m {
        ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. }
            if c.text == "you are all idiots" =>
        {
            Some(c.id)
        }
        _ => None,
    });
    alice.send(&mut w, &ClientMessage::Chat("mine".to_string()));
    let own_id = alice.wait_for(&mut w, &mut [&mut admin, &mut bob], |m| match m {
        ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "mine" => {
            Some(c.id)
        }
        _ => None,
    });

    assert_eq!(
        report(
            &mut alice,
            &mut w,
            &mut [&mut admin, &mut bob],
            ReportTarget::Message {
                conv: ConvRef::General,
                msg_id,
            },
            ReportReason::Harassment,
        ),
        Ok(())
    );
    // The moderator online sees it at once, with the text.
    let line = admin.wait_for(&mut w, &mut [&mut alice, &mut bob], |m| match m {
        ServerMessage::AuditLine { entry } if entry.action == "report.message" => {
            Some(entry.clone())
        }
        _ => None,
    });
    assert_eq!(line.actor.as_deref(), Some("alice"));
    assert_eq!(line.target.as_deref(), Some("bob"));
    let detail = line.detail.unwrap_or_default();
    assert!(
        detail.contains("harassment") && detail.contains("idiots"),
        "{detail}"
    );
    // Bob is not told.
    assert!(!bob
        .inbox
        .iter()
        .any(|m| matches!(m, ServerMessage::AuditLine { .. })));

    // Not one's own message, not oneself, not a message that is not there.
    let own = ReportTarget::Message {
        conv: ConvRef::General,
        msg_id: own_id,
    };
    assert_eq!(
        report(
            &mut alice,
            &mut w,
            &mut [&mut admin, &mut bob],
            own,
            ReportReason::Spam
        ),
        Err(ReportRefusal::Yourself)
    );
    assert_eq!(
        report(
            &mut alice,
            &mut w,
            &mut [&mut admin, &mut bob],
            ReportTarget::User { user: alice_user },
            ReportReason::Spam
        ),
        Err(ReportRefusal::Yourself)
    );
    assert_eq!(
        report(
            &mut alice,
            &mut w,
            &mut [&mut admin, &mut bob],
            ReportTarget::Message {
                conv: ConvRef::General,
                msg_id: 999_999,
            },
            ReportReason::Spam
        ),
        Err(ReportRefusal::NoSuchMessage)
    );
    assert_eq!(
        report(
            &mut alice,
            &mut w,
            &mut [&mut admin, &mut bob],
            ReportTarget::User { user: bob_user },
            ReportReason::Other
        ),
        Ok(())
    );

    // The Reports tab: both, newest first; members cannot read it.
    w.settle(
        &mut [&mut admin, &mut alice, &mut bob],
        Duration::from_millis(500),
    );
    bob.admin(&mut w, 1, AdminCommand::ListReports { resolved: false });
    assert_eq!(
        bob.admin_result(&mut w, &mut [&mut admin, &mut alice], 1),
        Err(AdminError::NoPermission)
    );
    // The reports are written by the persistence thread, in real time;
    // the settle above is simulated time, which a loaded machine runs
    // through before the second write is even queued. Ask until both
    // are there (this was a flake: one report listed, about one run in
    // four on a busy disk).
    let mut list = Vec::new();
    for attempt in 0..200u32 {
        // Ids of their own: the requests further down are 3, 4 and 5.
        let request = 100 + attempt;
        admin.admin(
            &mut w,
            request,
            AdminCommand::ListReports { resolved: false },
        );
        let Ok(AdminOutcome::Reports(now)) =
            admin.admin_result(&mut w, &mut [&mut alice, &mut bob], request)
        else {
            panic!("no reports");
        };
        list = now;
        if list.len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].msg_id, None);
    assert_eq!(list[1].msg_id, Some(msg_id));
    assert_eq!(list[1].excerpt, "you are all idiots");
    assert_eq!(list[1].reporter.as_deref(), Some("alice"));
    assert_eq!(list[1].target_user, Some(bob_user));
    assert_eq!(list[1].note, "seen it twice");
    assert!(!list[1].in_dm);

    // Handled: out of the open list, still in the full one.
    admin.admin(&mut w, 3, AdminCommand::ResolveReport { id: list[1].id });
    assert_eq!(
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 3),
        Ok(AdminOutcome::Done)
    );
    w.settle(
        &mut [&mut admin, &mut alice, &mut bob],
        Duration::from_millis(500),
    );
    admin.admin(&mut w, 4, AdminCommand::ListReports { resolved: false });
    let Ok(AdminOutcome::Reports(open)) =
        admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 4)
    else {
        panic!("no reports");
    };
    assert_eq!(open.len(), 1);
    admin.admin(&mut w, 5, AdminCommand::ListReports { resolved: true });
    let Ok(AdminOutcome::Reports(all)) = admin.admin_result(&mut w, &mut [&mut alice, &mut bob], 5)
    else {
        panic!("no reports");
    };
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|r| r.resolved && r.msg_id == Some(msg_id)));
}

#[test]
fn a_dm_can_only_be_reported_by_someone_in_it() {
    use protocol::{ReportReason, ReportRefusal, ReportTarget};
    let dir = temp_dir("report-dm");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let mut carol = w.connect_member("carol");
    let alice_user = alice.key.user_id();
    let bob_user = bob.key.user_id();

    dm(&mut alice, &mut w, bob_user, "send me your password", 1);
    let msg_id = dm_verdict(&mut alice, &mut w, &mut [&mut bob, &mut carol], 1).expect("stored");

    // Carol guesses the id: not her conversation.
    assert_eq!(
        report(
            &mut carol,
            &mut w,
            &mut [&mut alice, &mut bob],
            ReportTarget::Message {
                conv: ConvRef::Dm(alice_user),
                msg_id,
            },
            ReportReason::Spam
        ),
        Err(ReportRefusal::NoSuchMessage)
    );
    // Bob is in it.
    assert_eq!(
        report(
            &mut bob,
            &mut w,
            &mut [&mut alice, &mut carol],
            ReportTarget::Message {
                conv: ConvRef::Dm(alice_user),
                msg_id,
            },
            ReportReason::Illegal
        ),
        Ok(())
    );
    w.settle(
        &mut [&mut alice, &mut bob, &mut carol],
        Duration::from_millis(500),
    );
    assert_eq!(
        rows_become(&dir, "reports", "reporter = ?1", &bob_user, 1),
        1
    );
    let conn = rusqlite::Connection::open(dir.join("baucord.db")).expect("open db");
    let (excerpt, in_dm): (String, i64) = conn
        .query_row("SELECT excerpt, in_dm FROM reports", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("one report");
    assert_eq!(excerpt, "send me your password");
    assert_eq!(in_dm, 1);
}

#[test]
fn deleting_your_account_removes_it_and_everything_it_posted() {
    use protocol::{ReportReason, ReportTarget};
    let dir = temp_dir("delete-account");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    let bob_user = bob.key.user_id();

    alice.send(&mut w, &ClientMessage::Chat("bye all".to_string()));
    let general_id = bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::Chat(c) | ServerMessage::ChatFrom { msg: c, .. } if c.text == "bye all" => {
            Some(c.id)
        }
        _ => None,
    });
    dm(&mut alice, &mut w, bob_user, "between us", 1);
    dm_verdict(&mut alice, &mut w, &mut [&mut bob], 1).expect("stored");
    dm(&mut bob, &mut w, alice_user, "sure", 2);
    dm_verdict(&mut bob, &mut w, &mut [&mut alice], 2).expect("stored");
    set_block(&mut alice, &mut w, &mut [&mut bob], bob_user, true);
    report(
        &mut bob,
        &mut w,
        &mut [&mut alice],
        ReportTarget::User { user: alice_user },
        ReportReason::Spam,
    )
    .expect("filed");
    report(
        &mut alice,
        &mut w,
        &mut [&mut bob],
        ReportTarget::User { user: bob_user },
        ReportReason::Spam,
    )
    .expect("filed");
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));
    assert_eq!(
        rows_become(&dir, "reports", "1 = 1 OR ?1 IS NULL", &alice_user, 2),
        2
    );
    assert!(stored_rows(&dir, "messages", "user_id = ?1", &alice_user) >= 2);

    alice.send(&mut w, &ClientMessage::DeleteAccount);
    alice.wait_for(&mut w, &mut [&mut bob], |m| {
        matches!(m, ServerMessage::AccountDeleted).then_some(())
    });
    // Her message leaves Bob's screen now.
    bob.wait_for(&mut w, &mut [&mut alice], |m| match m {
        ServerMessage::MessageDeleted { msg_id } if *msg_id == general_id => Some(()),
        _ => None,
    });
    w.settle(&mut [&mut alice, &mut bob], Duration::from_secs(1));
    assert!(!w.server().policy().users.contains_key(&alice_user));
    assert!(w.server().policy().blocks.is_empty());

    // On disk: nothing of hers, the DM conversation whole, her blocks,
    // the report about her; Bob's report by her stays, without her.
    assert_eq!(
        rows_become(&dir, "users", "user_id = ?1", &alice_user, 0),
        0
    );
    assert_eq!(
        stored_rows(&dir, "messages", "user_id = ?1", &alice_user),
        0
    );
    assert_eq!(stored_rows(&dir, "messages", "user_id = ?1", &bob_user), 0);
    assert_eq!(
        stored_rows(
            &dir,
            "conversations",
            "kind = 1 AND (user_a = ?1 OR user_b = ?1)",
            &alice_user
        ),
        0
    );
    assert_eq!(
        stored_rows(&dir, "blocks", "blocker = ?1 OR blocked = ?1", &alice_user),
        0
    );
    assert_eq!(
        stored_rows(&dir, "reports", "target_user = ?1", &alice_user),
        0
    );
    assert_eq!(
        stored_rows(&dir, "reports", "reporter = ?1", &alice_user),
        0
    );
    assert_eq!(
        stored_rows(&dir, "reports", "target_user = ?1", &bob_user),
        1
    );
    // The record keeps what happened, not who she was.
    assert_eq!(
        stored_rows(&dir, "audit", "actor_user_id = ?1", &alice_user),
        0
    );
    assert!(
        stored_rows(
            &dir,
            "audit",
            "action = 'user.delete' AND ?1 IS NOT NULL",
            &alice_user
        ) >= 1
    );

    // The same device comes back as nobody: it needs a new invite.
    let again = w.try_connect_with_key("alice", Auth::Key, alice.key.clone());
    assert!(
        matches!(again, Err(RefusedReason::NeedInvite { .. })),
        "{again:?}"
    );
}

#[test]
fn a_deleted_account_takes_the_files_of_its_conversations_with_it() {
    let dir = temp_dir("delete-account-files");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let alice_user = alice.key.user_id();
    // Bob posts one file to Alice and another to everyone.
    let (_, in_dm) = post_file(
        &mut bob,
        &mut w,
        &mut [&mut alice],
        1,
        "for-alice.txt",
        "text/plain",
        &b"only for alice ".repeat(30),
        ConvRef::Dm(alice_user),
    )
    .expect("posted to the DM");
    let (_, in_general) = post_file(
        &mut bob,
        &mut w,
        &mut [&mut alice],
        2,
        "for-all.txt",
        "text/plain",
        &b"for everyone ".repeat(30),
        ConvRef::General,
    )
    .expect("posted to General");
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));
    assert_eq!(rows_become(&dir, "blobs", "blob_id = ?1", &in_dm, 1), 1);

    alice.send(&mut w, &ClientMessage::DeleteAccount);
    alice.wait_for(&mut w, &mut [&mut bob], |m| {
        matches!(m, ServerMessage::AccountDeleted).then_some(())
    });
    // The conversation went whole, so its file has nothing pointing at
    // it any more: gone from the server and the disk. Bob's post in
    // General stays.
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().policy().blobs.contains_key(&in_dm) {
        assert!(Instant::now() < deadline, "the DM's file stayed");
        w.settle(&mut [&mut bob], Duration::from_millis(100));
    }
    assert_eq!(rows_become(&dir, "blobs", "blob_id = ?1", &in_dm, 0), 0);
    assert!(w.server().policy().blobs.contains_key(&in_general));
    assert_eq!(stored_rows(&dir, "blobs", "blob_id = ?1", &in_general), 1);
}

#[test]
fn a_picture_someone_else_also_uses_outlives_its_uploaders_account() {
    let dir = temp_dir("delete-account-shared");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect_member("alice");
    let mut bob = w.connect_member("bob");
    let bob_user = bob.key.user_id();
    let picture = png_blob(7, 6 * 1024);
    // Alice uploads it first; Bob's identical upload is the same file.
    let use_as_avatar = |who: &mut TestClient, other: &mut TestClient, w: &mut World| {
        let blob = who.upload_blob(w, 1, "image/png", &picture);
        if !who
            .inbox
            .iter()
            .any(|m| matches!(m, ServerMessage::UploadAccept { needed: false, .. }))
        {
            who.wait_for(w, &mut [&mut *other], |m| match m {
                ServerMessage::UploadDone { upload_id: 1, .. } => Some(()),
                ServerMessage::UploadRejected { reason, .. } => panic!("rejected: {reason:?}"),
                _ => None,
            });
        }
        set_profile(who, w, None, Some(blob));
        profile_verdict(who, w, &mut [&mut *other]).expect("avatar set");
    };
    use_as_avatar(&mut alice, &mut bob, &mut w);
    use_as_avatar(&mut bob, &mut alice, &mut w);
    let blob = transport::sha256::digest(&picture);
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));

    alice.send(&mut w, &ClientMessage::DeleteAccount);
    alice.wait_for(&mut w, &mut [&mut bob], |m| {
        matches!(m, ServerMessage::AccountDeleted).then_some(())
    });
    w.settle(&mut [&mut bob], Duration::from_secs(1));
    // Bob's avatar is still there, now nobody's upload.
    let meta = w.server().policy().blobs.get(&blob).cloned().expect("kept");
    assert_eq!(meta.created_by, None);
    assert!(meta.refs >= 1);
    assert_eq!(
        rows_become(
            &dir,
            "blobs",
            "blob_id = ?1 AND created_by IS NULL",
            &blob,
            1
        ),
        1
    );
    assert_eq!(
        w.server()
            .policy()
            .users
            .get(&bob_user)
            .and_then(|u| u.profile.avatar),
        Some(blob)
    );
}

#[test]
fn the_last_admin_cannot_delete_their_account() {
    let mut w = World::new(test_config(vec![open_channel("Lobby", true)]));
    let boot = w.bootstrap_code();
    let mut admin = w.connect_as("mpw", Auth::Invite { code: boot });
    admin.send(&mut w, &ClientMessage::DeleteAccount);
    let reason = admin.wait_for(&mut w, &mut [], |m| match m {
        ServerMessage::AccountDeleteRefused { reason } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, protocol::DeleteRefusal::LastAdmin);
    let admin_user = admin.key.user_id();
    assert!(w.server().policy().users.contains_key(&admin_user));
}

/// Wait until the persistence thread has written everything queued, so
/// the CLI finds the rows.
fn wait_for_rows(w: &mut World) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.server().persistence().stats().queued > 0 {
        assert!(Instant::now() < deadline, "rows never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What a policy reload sends a client: the roster lines (whose), and
/// how many `Session` and `RoleList` messages.
fn reload_messages(c: &TestClient) -> (Vec<ClientId>, usize, usize) {
    let mut details = Vec::new();
    let (mut sessions, mut role_lists) = (0, 0);
    for m in &c.inbox {
        match m {
            ServerMessage::UserDetails { client_id, .. } => details.push(*client_id),
            ServerMessage::Session { .. } => sessions += 1,
            ServerMessage::RoleList { .. } => role_lists += 1,
            _ => {}
        }
    }
    (details, sessions, role_lists)
}

#[test]
fn cli_listings_reload_nothing_and_message_nobody() {
    // Every CLI open re-synced the built-in roles and truncated the WAL
    // on close, so `server user list` read as a change: the server
    // reloaded every table and sent N² messages (review P1-22).
    let dir = temp_dir("cli-listings");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    // Bob's arrival still on its way to alice.
    w.settle(&mut [&mut alice, &mut bob], Duration::from_millis(500));
    wait_for_rows(&mut w);
    let before = w.server().persistence().stats().external_changes;
    alice.inbox.clear();
    bob.inbox.clear();

    let db = dir.join("baucord.db");
    let (found, policy) = server::persistence::cli_find_user(&db, "alice").expect("by name");
    assert_eq!(found, alice.key.user_id());
    assert_eq!(policy.users.len(), 2);
    server::persistence::cli_load_policy(&db).expect("user list");
    assert!(!server::persistence::cli_list_invites(&db)
        .expect("invite list")
        .is_empty());
    server::persistence::cli_telemetry(&db, &found, 0, 10).expect("telemetry");
    assert!(server::persistence::check(&db).expect("db check").is_ok());

    // Past the 2 s poll, twice.
    let until = Instant::now() + Duration::from_millis(4500);
    while Instant::now() < until {
        w.pump(&mut [&mut alice, &mut bob]);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(w.server().persistence().stats().external_changes, before);
    for c in [&alice, &bob] {
        assert_eq!(reload_messages(c), (Vec::new(), 0, 0), "{:?}", c.inbox);
    }
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cli_grant_messages_only_what_changed() {
    let dir = temp_dir("cli-grant-diff");
    let mut w = World::new(persistent_config(vec![open_channel("Lobby", true)], &dir));
    let mut alice = w.connect("alice");
    let mut bob = w.connect("bob");
    let mut carol = w.connect("carol");
    // The arrivals still on their way to the others.
    w.settle(
        &mut [&mut alice, &mut bob, &mut carol],
        Duration::from_millis(500),
    );
    wait_for_rows(&mut w);
    for c in [&mut alice, &mut bob, &mut carol] {
        c.inbox.clear();
    }

    let db = dir.join("baucord.db");
    let alice_user = alice.key.user_id();
    server::persistence::cli_set_roles(&db, &alice_user, &[ROLE_MEMBER]).expect("grant");
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        w.pump(&mut [&mut alice, &mut bob, &mut carol]);
        if alice.inbox.iter().any(|m| {
            matches!(m, ServerMessage::Session { permissions, .. }
                if *permissions == permissions::MEMBER_PERMISSIONS)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the grant never reached the session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    w.settle(
        &mut [&mut alice, &mut bob, &mut carol],
        Duration::from_millis(200),
    );

    // Alice's roster line to everyone, her own permission set to her,
    // and the role table to nobody: it did not change.
    let alice_id = alice.id;
    assert_eq!(reload_messages(&alice), (vec![alice_id], 1, 0));
    assert_eq!(reload_messages(&bob), (vec![alice_id], 0, 0));
    assert_eq!(reload_messages(&carol), (vec![alice_id], 0, 0));
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}
