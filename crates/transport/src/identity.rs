//! Server identity and client-side pinning (spec §4.6).
//!
//! The server owns one Ed25519 key. On every start it regenerates a
//! self-signed certificate for it (cheap; the *key* is what is pinned)
//! and logs the [`Fingerprint`] — `SHA256:` + base64 of SHA-256 over the
//! SPKI DER, SSH's display format. Expiry and CA chains play no part.
//!
//! The client pins the fingerprint per server entry. Its rustls verifier
//! ([`PinnedVerifier`]) accepts a certificate iff the SPKI hash equals
//! the pin, **or** the certificate carries a valid successor proof from
//! the pinned key ([`SUCCESSOR_EXTENSION_OID`]), or there is no pin yet
//! (trust on first use). Mismatch without proof is refused inside the
//! TLS handshake, before a password could ever be sent.
//!
//! Rotation (`baucord-server rotate-key`) moves the current key to
//! `retired/` and issues the new certificate with, for each retired key
//! still within [`RETIRED_KEY_RETENTION`], `(retired_spki, signature by
//! the retired key over the new spki)`. A client pinned to a retired key
//! re-pins silently.

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use base64::Engine;
use log::{info, warn};
use rcgen::{CertificateParams, CustomExtension, KeyPair, PKCS_ED25519, PublicKeyData, SigningKey};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

use crate::der;

/// X.509 extension carrying the successor proofs. Under the UUID arc
/// (`2.25`): a random 63-bit identifier, the largest a single arc can
/// hold in the certificate builder. Unique by construction, needs no
/// IANA registration, and **must never change once shipped** — every
/// deployed client parses it.
pub const SUCCESSOR_EXTENSION_OID: &[u64] = &[2, 25, 3_276_867_917_789_066_850];

/// How long a retired key keeps vouching for its successor **[default:
/// 2 years]**. A client that hasn't connected in longer than this gets
/// the identity prompt.
pub const RETIRED_KEY_RETENTION: Duration = Duration::from_secs(2 * 365 * 24 * 3600);

/// Certificate validity **[default: 20 years]**. Cosmetic — the client
/// ignores expiry.
const CERT_VALIDITY_YEARS: i32 = 20;
/// Mean Gregorian year, for turning the wall clock into a year number.
const SECONDS_PER_YEAR: u64 = 31_556_952;

/// TLS exporter label for the client-key proof (Part C). Stable forever.
pub const CLIENT_AUTH_EXPORTER_LABEL: &[u8] = b"baucord-client-auth";
/// Length of the exported value the client signs.
pub const CLIENT_AUTH_EXPORTER_BYTES: usize = 32;

const KEY_FILE: &str = "identity.key";
const CERT_FILE: &str = "identity.pem";
const RETIRED_DIR: &str = "retired";
const SERVER_NAME: &str = "baucord";

// ── Fingerprint ─────────────────────────────────────────────────────────

/// SHA-256 over a `SubjectPublicKeyInfo` DER — the pinned identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    #[must_use]
    pub fn of_spki(spki_der: &[u8]) -> Self {
        Self(Sha256::digest(spki_der).into())
    }

    /// The SPKI hash of a certificate (DER), if it can be walked.
    #[must_use]
    pub fn of_certificate(cert_der: &[u8]) -> Option<Self> {
        der::parse_certificate(cert_der).map(|c| Self::of_spki(c.spki))
    }

    /// Filesystem-safe form (URL-safe base64, no padding).
    #[must_use]
    pub fn file_stem(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.0)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(self.0)
        )
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::str::FromStr for Fingerprint {
    type Err = ();

    /// Accepts `SHA256:<base64>` with or without padding.
    fn from_str(s: &str) -> Result<Self, ()> {
        let body = s.trim().strip_prefix("SHA256:").ok_or(())?;
        let body = body.trim_end_matches('=');
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(body)
            .map_err(|_| ())?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| ())?;
        Ok(Self(arr))
    }
}

// ── Successor proofs ────────────────────────────────────────────────────

/// One retired key vouching for the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessorProof {
    /// The retired key's SPKI DER (the client hashes it to match its pin
    /// and uses the key inside to check the signature).
    pub retired_spki: Vec<u8>,
    /// Ed25519 signature by the retired key over the *new* SPKI DER.
    pub signature: Vec<u8>,
}

/// Extension content: `[count: u8]` then per proof
/// `[spki_len: u16 LE][spki][sig_len: u16 LE][sig]`.
#[must_use]
pub fn encode_successor_proofs(proofs: &[SuccessorProof]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(u8::try_from(proofs.len()).unwrap_or(u8::MAX));
    for p in proofs.iter().take(usize::from(u8::MAX)) {
        for field in [&p.retired_spki, &p.signature] {
            let len = u16::try_from(field.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&field[..usize::from(len)]);
        }
    }
    out
}

#[must_use]
pub fn decode_successor_proofs(bytes: &[u8]) -> Option<Vec<SuccessorProof>> {
    let (&count, mut rest) = bytes.split_first()?;
    let mut proofs = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let (spki, after) = read_field(rest)?;
        let (sig, after) = read_field(after)?;
        rest = after;
        proofs.push(SuccessorProof {
            retired_spki: spki.to_vec(),
            signature: sig.to_vec(),
        });
    }
    rest.is_empty().then_some(proofs)
}

fn read_field(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = input.split_first_chunk::<2>()?;
    let len = usize::from(u16::from_le_bytes(*len));
    (rest.len() >= len).then(|| rest.split_at(len))
}

/// Check one proof against a pin: the retired SPKI must hash to `pin`
/// and its key must have signed `new_spki`.
fn proof_vouches(proof: &SuccessorProof, pin: &Fingerprint, new_spki: &[u8]) -> bool {
    if Fingerprint::of_spki(&proof.retired_spki) != *pin {
        return false;
    }
    let Some(key) = der::spki_public_key(&proof.retired_spki) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(new_spki, &proof.signature)
        .is_ok()
}

/// Verify an Ed25519 signature with a raw 32-byte public key (client
/// key proofs, Part C).
#[must_use]
pub fn verify_ed25519(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(message, signature)
        .is_ok()
}

// ── Client key ──────────────────────────────────────────────────────────

/// The client's own Ed25519 key (spec §3.3): generated on first use,
/// stored PKCS#8 next to the settings file, and used to sign the TLS
/// exporter in `Hello` so the server can derive a stable user id. Never
/// leaves the machine.
pub struct ClientKey {
    key: ring::signature::Ed25519KeyPair,
}

impl std::fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientKey")
            .field("public_key", &hex_short(&self.public_key()))
            .finish_non_exhaustive()
    }
}

impl ClientKey {
    /// Load the key at `path`, or generate one there.
    ///
    /// # Errors
    /// I/O failure, or an unreadable existing key (never overwritten).
    pub fn load_or_create(path: &Path) -> Result<Self, IdentityError> {
        if path.exists() {
            let der = fs::read(path).map_err(|e| IdentityError::Io(path.to_path_buf(), e))?;
            return Self::from_pkcs8(&der)
                .map_err(|e| IdentityError::ClientKey(path.to_path_buf(), e));
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| IdentityError::Io(dir.to_path_buf(), e))?;
        }
        let rng = ring::rand::SystemRandom::new();
        let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| IdentityError::ClientKey(path.to_path_buf(), "keygen failed".into()))?;
        write_private(path, doc.as_ref())?;
        info!("Generated client key at {}", path.display());
        Self::from_pkcs8(doc.as_ref()).map_err(|e| IdentityError::ClientKey(path.to_path_buf(), e))
    }

    /// # Errors
    /// Not a PKCS#8 Ed25519 key.
    pub fn from_pkcs8(der: &[u8]) -> Result<Self, String> {
        ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(der)
            .map(|key| Self { key })
            .map_err(|e| format!("not an Ed25519 PKCS#8 key: {e}"))
    }

    /// The raw 32-byte public key.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        use ring::signature::KeyPair as _;
        let mut out = [0u8; 32];
        out.copy_from_slice(self.key.public_key().as_ref());
        out
    }

    /// Sign `message` (the TLS exporter); 64 bytes.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.key.sign(message).as_ref().to_vec()
    }

    /// The user id the server derives: SHA-256 of the public key.
    #[must_use]
    pub fn user_id(&self) -> [u8; 32] {
        Sha256::digest(self.public_key()).into()
    }
}

fn hex_short(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().take(8).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ── Server identity ─────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity directory {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("identity key {0}: {1}")]
    Key(PathBuf, rcgen::Error),
    #[error("client key {0}: {1}")]
    ClientKey(PathBuf, String),
    #[error("certificate generation: {0}")]
    Certificate(rcgen::Error),
    #[error("TLS configuration: {0}")]
    Tls(rustls::Error),
}

/// The server's key, its current certificate, and the retired keys that
/// vouch for it.
pub struct ServerIdentity {
    key: KeyPair,
    cert_der: Vec<u8>,
    fingerprint: Fingerprint,
    retired: Vec<KeyPair>,
}

impl ServerIdentity {
    /// A fresh in-memory identity (tests and the simulation harness).
    ///
    /// # Panics
    /// Only if the crypto provider cannot generate an Ed25519 key,
    /// which would mean `ring` is broken.
    #[must_use]
    pub fn generate() -> Self {
        let key = KeyPair::generate_for(&PKCS_ED25519).expect("Ed25519 keygen");
        Self::from_key(key, Vec::new()).expect("self-signed certificate")
    }

    /// Build from a key and the retired keys that should vouch for it.
    ///
    /// # Errors
    /// Certificate generation failure.
    pub fn from_key(key: KeyPair, retired: Vec<KeyPair>) -> Result<Self, IdentityError> {
        let cert_der = issue_certificate(&key, &retired)?;
        let fingerprint = Fingerprint::of_spki(&key.subject_public_key_info());
        Ok(Self {
            key,
            cert_der,
            fingerprint,
            retired,
        })
    }

    /// Load `<dir>/identity.key` (creating the directory and a new key
    /// if absent), load `<dir>/retired/*.key` within retention, and
    /// (re)write `<dir>/identity.pem`. Returns whether a key was created.
    ///
    /// # Errors
    /// Filesystem or key-parse failures; a corrupt key file is an error,
    /// never silently replaced.
    pub fn load_or_create(dir: &Path) -> Result<(Self, bool), IdentityError> {
        fs::create_dir_all(dir).map_err(|e| IdentityError::Io(dir.to_path_buf(), e))?;
        let key_path = dir.join(KEY_FILE);
        let (key, created) = if key_path.exists() {
            (load_key(&key_path)?, false)
        } else {
            let key = KeyPair::generate_for(&PKCS_ED25519).map_err(IdentityError::Certificate)?;
            write_private(&key_path, key.serialize_pem().as_bytes())?;
            info!(
                "Generated new server identity key at {}",
                key_path.display()
            );
            (key, true)
        };
        let retired = load_retired(&dir.join(RETIRED_DIR));
        let identity = Self::from_key(key, retired)?;
        let cert_path = dir.join(CERT_FILE);
        fs::write(&cert_path, identity.cert_pem())
            .map_err(|e| IdentityError::Io(cert_path.clone(), e))?;
        Ok((identity, created))
    }

    /// Retire the current key into `<dir>/retired/` and generate a new
    /// one whose certificate carries a successor proof from every
    /// retired key still within retention.
    ///
    /// # Errors
    /// Filesystem or key failures. The old key is moved only after the
    /// new one is safely written.
    pub fn rotate(dir: &Path) -> Result<Self, IdentityError> {
        let (current, _) = Self::load_or_create(dir)?;
        let retired_dir = dir.join(RETIRED_DIR);
        fs::create_dir_all(&retired_dir).map_err(|e| IdentityError::Io(retired_dir.clone(), e))?;

        let new_key = KeyPair::generate_for(&PKCS_ED25519).map_err(IdentityError::Certificate)?;
        let key_path = dir.join(KEY_FILE);
        let staged = dir.join("identity.key.new");
        write_private(&staged, new_key.serialize_pem().as_bytes())?;

        let retired_path = retired_dir.join(format!("{}.key", current.fingerprint.file_stem()));
        fs::rename(&key_path, &retired_path)
            .map_err(|e| IdentityError::Io(retired_path.clone(), e))?;
        fs::rename(&staged, &key_path).map_err(|e| IdentityError::Io(key_path.clone(), e))?;
        info!(
            "Rotated server identity: {} retired to {}",
            current.fingerprint,
            retired_path.display()
        );
        Ok(Self::load_or_create(dir)?.0)
    }

    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    #[must_use]
    pub fn cert_pem(&self) -> String {
        let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.cert_der);
        for line in b64.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(line).unwrap_or(""));
            out.push('\n');
        }
        out.push_str("-----END CERTIFICATE-----\n");
        out
    }

    /// Fingerprints of the retired keys currently vouching.
    #[must_use]
    pub fn retired_fingerprints(&self) -> Vec<Fingerprint> {
        self.retired
            .iter()
            .map(|k| Fingerprint::of_spki(&k.subject_public_key_info()))
            .collect()
    }

    /// The rustls server configuration: TLS 1.3 only, our ALPN, key log
    /// honoring `SSLKEYLOGFILE`.
    ///
    /// # Errors
    /// rustls rejects the key/certificate pair (cannot happen for a key
    /// we just generated, but the error is propagated rather than
    /// unwrapped).
    pub fn server_crypto(&self) -> Result<Arc<rustls::ServerConfig>, IdentityError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.serialize_der()));
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(IdentityError::Tls)?
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(self.cert_der.clone())], key)
            .map_err(IdentityError::Tls)?;
        config.alpn_protocols = vec![protocol::ALPN.to_vec()];
        config.key_log = Arc::new(rustls::KeyLogFile::new());
        config.max_early_data_size = 0;
        Ok(Arc::new(config))
    }
}

fn issue_certificate(key: &KeyPair, retired: &[KeyPair]) -> Result<Vec<u8>, IdentityError> {
    let mut params = CertificateParams::new(vec![SERVER_NAME.to_string()])
        .map_err(IdentityError::Certificate)?;
    // Validity is cosmetic (the client ignores it) and the certificate
    // is regenerated on every start, so whole years are precise enough
    // and keep the `time` crate out of our API.
    let year = 1970
        + i32::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                / SECONDS_PER_YEAR,
        )
        .unwrap_or(2026);
    params.not_before = rcgen::date_time_ymd(year - 1, 1, 1);
    params.not_after = rcgen::date_time_ymd(year + CERT_VALIDITY_YEARS, 1, 1);

    let new_spki = key.subject_public_key_info();
    let mut proofs = Vec::with_capacity(retired.len());
    for old in retired {
        let signature = old.sign(&new_spki).map_err(IdentityError::Certificate)?;
        proofs.push(SuccessorProof {
            retired_spki: old.subject_public_key_info(),
            signature,
        });
    }
    if !proofs.is_empty() {
        params
            .custom_extensions
            .push(CustomExtension::from_oid_content(
                SUCCESSOR_EXTENSION_OID,
                encode_successor_proofs(&proofs),
            ));
    }
    let cert = params
        .self_signed(key)
        .map_err(IdentityError::Certificate)?;
    Ok(cert.der().to_vec())
}

fn load_key(path: &Path) -> Result<KeyPair, IdentityError> {
    let pem = fs::read_to_string(path).map_err(|e| IdentityError::Io(path.to_path_buf(), e))?;
    let key = KeyPair::from_pem(&pem).map_err(|e| IdentityError::Key(path.to_path_buf(), e))?;
    info!("Loaded server identity key from {}", path.display());
    Ok(key)
}

fn load_retired(dir: &Path) -> Vec<KeyPair> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("key") {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .unwrap_or(Duration::ZERO);
        if age > RETIRED_KEY_RETENTION {
            info!(
                "Retired key {} is older than the retention window; no longer vouching",
                path.display()
            );
            continue;
        }
        match load_key(&path) {
            Ok(key) => keys.push(key),
            Err(e) => warn!("Skipping unreadable retired key: {e}"),
        }
    }
    keys
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), IdentityError> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| IdentityError::Io(path.to_path_buf(), e))?;
    file.write_all(bytes)
        .map_err(|e| IdentityError::Io(path.to_path_buf(), e))
}

// ── Client-side pinning ─────────────────────────────────────────────────

/// How the presented certificate was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Matched the pin.
    Pinned,
    /// No pin yet — trust on first use; the caller stores the fingerprint.
    FirstUse,
    /// A successor proof from the pinned key; the caller re-pins silently.
    Successor { from: Fingerprint },
}

/// What the verifier saw during the last handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Accepted {
        fingerprint: Fingerprint,
        trust: Trust,
    },
    /// Refused: identity changed with no valid successor proof.
    Rejected {
        expected: Fingerprint,
        observed: Fingerprint,
    },
}

/// A rustls verifier that pins a server public key instead of walking
/// a CA chain.
#[derive(Debug)]
pub struct PinnedVerifier {
    pin: Option<Fingerprint>,
    observation: Mutex<Option<Observation>>,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    #[must_use]
    pub fn new(pin: Option<Fingerprint>) -> Arc<Self> {
        Arc::new(Self {
            pin,
            observation: Mutex::new(None),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        })
    }

    /// The pin this verifier enforces.
    #[must_use]
    pub fn pin(&self) -> Option<Fingerprint> {
        self.pin
    }

    /// What the last handshake presented (set once per handshake).
    #[must_use]
    pub fn observation(&self) -> Option<Observation> {
        *self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The rustls client configuration built around this verifier: TLS
    /// 1.3 only, our ALPN, key log honoring `SSLKEYLOGFILE`.
    ///
    /// # Errors
    /// Only if rustls refuses the protocol-version selection.
    pub fn client_crypto(self: &Arc<Self>) -> Result<Arc<rustls::ClientConfig>, IdentityError> {
        let mut config = rustls::ClientConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(IdentityError::Tls)?
            .dangerous()
            .with_custom_certificate_verifier(self.clone())
            .with_no_client_auth();
        config.alpn_protocols = vec![protocol::ALPN.to_vec()];
        config.key_log = Arc::new(rustls::KeyLogFile::new());
        config.enable_early_data = false;
        Ok(Arc::new(config))
    }

    /// The server name to dial with; the verifier ignores it.
    #[must_use]
    pub fn server_name() -> &'static str {
        SERVER_NAME
    }

    fn record(&self, obs: Observation) {
        *self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(obs);
    }

    /// Decide on a presented certificate. Pure; exposed for tests.
    #[must_use]
    pub fn decide(&self, cert_der: &[u8]) -> Option<Observation> {
        let cert = der::parse_certificate(cert_der)?;
        let fingerprint = Fingerprint::of_spki(cert.spki);
        let Some(pin) = self.pin else {
            return Some(Observation::Accepted {
                fingerprint,
                trust: Trust::FirstUse,
            });
        };
        if fingerprint == pin {
            return Some(Observation::Accepted {
                fingerprint,
                trust: Trust::Pinned,
            });
        }
        let oid = der::encode_oid(SUCCESSOR_EXTENSION_OID);
        let proofs = cert
            .extensions
            .iter()
            .find(|e| e.oid == oid.as_slice())
            .and_then(|e| decode_successor_proofs(e.value))
            .unwrap_or_default();
        if proofs.iter().any(|p| proof_vouches(p, &pin, cert.spki)) {
            return Some(Observation::Accepted {
                fingerprint,
                trust: Trust::Successor { from: pin },
            });
        }
        Some(Observation::Rejected {
            expected: pin,
            observed: fingerprint,
        })
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Some(obs) = self.decide(end_entity.as_ref()) else {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadEncoding,
            ));
        };
        self.record(obs);
        match obs {
            Observation::Accepted { .. } => Ok(ServerCertVerified::assertion()),
            Observation::Rejected { .. } => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // QUIC is TLS 1.3 only.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_key_roundtrips_and_signs_verifiably() {
        let dir = std::env::temp_dir().join(format!("baucord-ck-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("identity.key");
        let key = ClientKey::load_or_create(&path).expect("create");
        let again = ClientKey::load_or_create(&path).expect("load");
        assert_eq!(key.public_key(), again.public_key());
        let sig = key.sign(b"exporter");
        assert!(verify_ed25519(&key.public_key(), b"exporter", &sig));
        assert!(!verify_ed25519(&key.public_key(), b"other", &sig));
        assert_eq!(key.user_id(), again.user_id());
        let _ = fs::remove_dir_all(&dir);
    }

    fn keypair() -> KeyPair {
        KeyPair::generate_for(&PKCS_ED25519).expect("keygen")
    }

    #[test]
    fn fingerprint_display_roundtrips() {
        let fp = Fingerprint([0xAB; 32]);
        let s = fp.to_string();
        assert!(s.starts_with("SHA256:"));
        assert_eq!(s.parse::<Fingerprint>(), Ok(fp));
        assert_eq!(format!("{s}=").parse::<Fingerprint>(), Ok(fp));
    }

    #[test]
    fn fingerprint_parse_rejects_garbage() {
        assert!("SHA256:zz".parse::<Fingerprint>().is_err());
        assert!("MD5:abc".parse::<Fingerprint>().is_err());
    }

    #[test]
    fn successor_proofs_roundtrip() {
        let proofs = vec![
            SuccessorProof {
                retired_spki: vec![1, 2, 3],
                signature: vec![4; 64],
            },
            SuccessorProof {
                retired_spki: vec![],
                signature: vec![5],
            },
        ];
        let bytes = encode_successor_proofs(&proofs);
        assert_eq!(decode_successor_proofs(&bytes), Some(proofs));
        assert_eq!(decode_successor_proofs(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode_successor_proofs(&[]), None);
    }

    #[test]
    fn identity_fingerprint_matches_its_certificate() {
        let id = ServerIdentity::generate();
        assert_eq!(
            Fingerprint::of_certificate(id.cert_der()),
            Some(id.fingerprint())
        );
    }

    #[test]
    fn verifier_should_trust_on_first_use_without_a_pin() {
        let id = ServerIdentity::generate();
        let v = PinnedVerifier::new(None);
        assert_eq!(
            v.decide(id.cert_der()),
            Some(Observation::Accepted {
                fingerprint: id.fingerprint(),
                trust: Trust::FirstUse
            })
        );
    }

    #[test]
    fn verifier_should_accept_the_pinned_key() {
        let id = ServerIdentity::generate();
        let v = PinnedVerifier::new(Some(id.fingerprint()));
        assert_eq!(
            v.decide(id.cert_der()),
            Some(Observation::Accepted {
                fingerprint: id.fingerprint(),
                trust: Trust::Pinned
            })
        );
    }

    #[test]
    fn verifier_should_reject_an_unrelated_key() {
        let pinned = ServerIdentity::generate();
        let other = ServerIdentity::generate();
        let v = PinnedVerifier::new(Some(pinned.fingerprint()));
        assert_eq!(
            v.decide(other.cert_der()),
            Some(Observation::Rejected {
                expected: pinned.fingerprint(),
                observed: other.fingerprint()
            })
        );
    }

    #[test]
    fn verifier_should_accept_a_signed_successor_and_report_the_old_pin() {
        let old_key = keypair();
        let old_fp = Fingerprint::of_spki(&old_key.subject_public_key_info());
        let new = ServerIdentity::from_key(keypair(), vec![old_key]).expect("identity");
        let v = PinnedVerifier::new(Some(old_fp));
        assert_eq!(
            v.decide(new.cert_der()),
            Some(Observation::Accepted {
                fingerprint: new.fingerprint(),
                trust: Trust::Successor { from: old_fp }
            })
        );
    }

    #[test]
    fn verifier_should_reject_a_successor_proof_from_the_wrong_key() {
        // The certificate vouches from key A; the client pinned key B.
        let a = keypair();
        let b = keypair();
        let b_fp = Fingerprint::of_spki(&b.subject_public_key_info());
        let new = ServerIdentity::from_key(keypair(), vec![a]).expect("identity");
        let v = PinnedVerifier::new(Some(b_fp));
        assert!(matches!(
            v.decide(new.cert_der()),
            Some(Observation::Rejected { .. })
        ));
    }

    #[test]
    fn forged_successor_proof_is_rejected() {
        // Correct retired SPKI (hash matches the pin) but a signature
        // made by a different key.
        let retired = keypair();
        let retired_fp = Fingerprint::of_spki(&retired.subject_public_key_info());
        let new_key = keypair();
        let forger = keypair();
        let proof = SuccessorProof {
            retired_spki: retired.subject_public_key_info(),
            signature: forger
                .sign(&new_key.subject_public_key_info())
                .expect("sign"),
        };
        assert!(!proof_vouches(
            &proof,
            &retired_fp,
            &new_key.subject_public_key_info()
        ));
    }

    #[test]
    fn rotate_should_retire_the_old_key_and_vouch_for_the_new_one() {
        let dir = std::env::temp_dir().join(format!("baucord-id-{}", rand::random::<u64>()));
        let (first, created) = ServerIdentity::load_or_create(&dir).expect("create");
        assert!(created);
        let (again, created) = ServerIdentity::load_or_create(&dir).expect("reload");
        assert!(!created);
        assert_eq!(again.fingerprint(), first.fingerprint());

        let rotated = ServerIdentity::rotate(&dir).expect("rotate");
        assert_ne!(rotated.fingerprint(), first.fingerprint());
        assert_eq!(rotated.retired_fingerprints(), vec![first.fingerprint()]);

        let v = PinnedVerifier::new(Some(first.fingerprint()));
        assert!(matches!(
            v.decide(rotated.cert_der()),
            Some(Observation::Accepted {
                trust: Trust::Successor { .. },
                ..
            })
        ));
        assert!(dir.join("identity.pem").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_and_client_crypto_configs_build() {
        let id = ServerIdentity::generate();
        assert!(id.server_crypto().is_ok());
        assert!(PinnedVerifier::new(None).client_crypto().is_ok());
    }
}
