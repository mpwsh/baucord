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
//! `retired/<stem>.<unix-secs>.key` and issues the new certificate with,
//! for each retired key still within [`RETIRED_KEY_RETENTION`],
//! `(retired_spki, signature by the retired key over the new spki)`. A
//! client pinned to a retired key re-pins silently. Retention counts from
//! the time in the file name, which survives copies and restores; a
//! `<stem>.key` retired by an older build falls back to the file's mtime.

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::Engine;
use log::info;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};

use crate::der;

// The server's side — its key files, its certificate, its retired keys
// — is its own module behind the `server` feature: it is the only user
// of `rcgen` (and through it `yasna` and `pem`), which every client
// used to compile for code it never runs (review P3-3).
#[cfg(any(test, feature = "server"))]
mod server;
#[cfg(any(test, feature = "server"))]
pub use server::{RetiredKey, ServerIdentity};

/// X.509 extension carrying the successor proofs. Under the UUID arc
/// (`2.25`): a random 63-bit identifier, the largest a single arc can
/// hold in the certificate builder. Unique by construction, needs no
/// IANA registration, and **must never change once shipped** — every
/// deployed client parses it.
pub const SUCCESSOR_EXTENSION_OID: &[u64] = &[2, 25, 3_276_867_917_789_066_850];

/// How long a retired key keeps vouching for its successor **[default:
/// 2 years]**, counted from its retirement. A client that hasn't
/// connected in longer than this gets the identity prompt.
pub const RETIRED_KEY_RETENTION: Duration = Duration::from_hours(2 * 365 * 24);

/// TLS exporter label for the client-key proof (Part C). Stable forever.
pub const CLIENT_AUTH_EXPORTER_LABEL: &[u8] = b"baucord-client-auth";
/// Length of the exported value the client signs.
pub const CLIENT_AUTH_EXPORTER_BYTES: usize = 32;

const SERVER_NAME: &str = "baucord";

// ── Fingerprint ─────────────────────────────────────────────────────────

/// SHA-256 over a `SubjectPublicKeyInfo` DER — the pinned identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    #[must_use]
    pub fn of_spki(spki_der: &[u8]) -> Self {
        Self(crate::sha256::digest(spki_der))
    }

    /// The SPKI hash of a certificate (DER), if it can be walked.
    #[cfg(test)]
    fn of_certificate(cert_der: &[u8]) -> Option<Self> {
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

    /// A fresh key that lives only in memory — for tests and for the
    /// simulator. The client's own key goes through
    /// [`Self::load_or_create`] so it survives restarts.
    ///
    /// # Panics
    /// If the system RNG fails, which `ring` treats as unrecoverable.
    #[must_use]
    pub fn generate() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let doc =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).expect("system RNG available");
        Self::from_pkcs8(doc.as_ref()).expect("freshly generated key parses")
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
        crate::sha256::digest(&self.public_key())
    }
}

fn hex_short(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().take(8).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity directory {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[cfg(any(test, feature = "server"))]
    #[error("identity key {0}: {1}")]
    Key(PathBuf, rcgen::Error),
    #[error("client key {0}: {1}")]
    ClientKey(PathBuf, String),
    #[cfg(any(test, feature = "server"))]
    #[error("certificate generation: {0}")]
    Certificate(rcgen::Error),
    #[error("TLS configuration: {0}")]
    Tls(rustls::Error),
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
    use std::time::SystemTime;

    use rcgen::{KeyPair, PublicKeyData, SigningKey, PKCS_ED25519};

    use super::{
        server::{retired_at_from_name, unix_secs, KEY_FILE, RETIRED_DIR},
        *,
    };

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

    const THREE_YEARS: Duration = Duration::from_hours(3 * 365 * 24);

    fn scratch_identity_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("baucord-{tag}-{}", rand::random::<u64>()))
    }

    fn set_mtime_ago(path: &Path, ago: Duration) {
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(SystemTime::now() - ago))
            .expect("set mtime");
    }

    fn only_retired_file(dir: &Path) -> PathBuf {
        let files: Vec<PathBuf> = fs::read_dir(dir.join(RETIRED_DIR))
            .expect("retired dir")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        files[0].clone()
    }

    #[test]
    fn a_key_made_years_ago_vouches_after_its_rotation() {
        // Operators rotate old keys; the rename used to keep the key's
        // creation time, and retention read that (review P1-15).
        let dir = scratch_identity_dir("id-old");
        let (first, _) = ServerIdentity::load_or_create(&dir).expect("create");
        set_mtime_ago(&dir.join(KEY_FILE), THREE_YEARS);

        let rotated = ServerIdentity::rotate(&dir).expect("rotate");
        assert_eq!(rotated.retired_fingerprints(), vec![first.fingerprint()]);
        let listed = ServerIdentity::retired_keys(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].fingerprint, first.fingerprint());
        assert!(!listed[0].from_mtime);
        assert!(listed[0].vouching);

        // A copy or restore that loses the date changes nothing: the
        // name carries it.
        set_mtime_ago(&only_retired_file(&dir), THREE_YEARS);
        let (again, created) = ServerIdentity::load_or_create(&dir).expect("reload");
        assert!(!created);
        assert_eq!(again.retired_fingerprints(), vec![first.fingerprint()]);
        let v = PinnedVerifier::new(Some(first.fingerprint()));
        assert!(matches!(
            v.decide(again.cert_der()),
            Some(Observation::Accepted {
                trust: Trust::Successor { .. },
                ..
            })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_counts_from_the_recorded_retirement() {
        let dir = scratch_identity_dir("id-expired");
        let (first, _) = ServerIdentity::load_or_create(&dir).expect("create");
        fs::create_dir_all(dir.join(RETIRED_DIR)).expect("retired dir");
        // Retired three years ago by its name; the fresh mtime is ignored.
        let then = unix_secs(SystemTime::now() - THREE_YEARS);
        let name = format!("{}.{then}.key", first.fingerprint().file_stem());
        fs::rename(dir.join(KEY_FILE), dir.join(RETIRED_DIR).join(name)).expect("retire");

        let (second, created) = ServerIdentity::load_or_create(&dir).expect("new key");
        assert!(created);
        assert!(second.retired_fingerprints().is_empty());
        assert_eq!(
            ServerIdentity::retired_keys(&dir),
            vec![RetiredKey {
                fingerprint: first.fingerprint(),
                retired_at: then,
                from_mtime: false,
                vouching: false,
            }]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_builds_retired_key_falls_back_to_its_mtime() {
        let dir = scratch_identity_dir("id-legacy");
        let (first, _) = ServerIdentity::load_or_create(&dir).expect("create");
        fs::create_dir_all(dir.join(RETIRED_DIR)).expect("retired dir");
        let legacy = dir
            .join(RETIRED_DIR)
            .join(format!("{}.key", first.fingerprint().file_stem()));
        fs::rename(dir.join(KEY_FILE), &legacy).expect("retire");

        let (second, _) = ServerIdentity::load_or_create(&dir).expect("new key");
        assert_eq!(second.retired_fingerprints(), vec![first.fingerprint()]);
        let listed = ServerIdentity::retired_keys(&dir);
        assert!(listed[0].from_mtime && listed[0].vouching);

        set_mtime_ago(&legacy, THREE_YEARS);
        let (third, _) = ServerIdentity::load_or_create(&dir).expect("reload");
        assert!(third.retired_fingerprints().is_empty());
        assert!(!ServerIdentity::retired_keys(&dir)[0].vouching);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_retirement_time_is_read_only_from_the_new_names() {
        assert_eq!(
            retired_at_from_name("ab_-XY.1700000000.key"),
            Some(1_700_000_000)
        );
        assert_eq!(retired_at_from_name("ab_-XY.key"), None);
        assert_eq!(retired_at_from_name("ab.soon.key"), None);
        assert_eq!(retired_at_from_name("ab.1700000000.pem"), None);
    }

    #[test]
    fn server_and_client_crypto_configs_build() {
        let id = ServerIdentity::generate();
        assert!(id.server_crypto().is_ok());
        assert!(PinnedVerifier::new(None).client_crypto().is_ok());
    }
}
