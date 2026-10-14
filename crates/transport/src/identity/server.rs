//! The server's identity (spec §5): its key on disk, the certificate it
//! presents, and the retired keys that vouch for the current one.
//! Compiled only with the `server` feature — a client verifies
//! certificates ([`super::PinnedVerifier`]) and never makes one.

use std::{fs, path::Path, sync::Arc, time::SystemTime};

use base64::Engine;
use log::{info, warn};
use rcgen::{CertificateParams, CustomExtension, KeyPair, PublicKeyData, SigningKey, PKCS_ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use super::{
    encode_successor_proofs, write_private, Fingerprint, IdentityError, SuccessorProof,
    RETIRED_KEY_RETENTION, SERVER_NAME, SUCCESSOR_EXTENSION_OID,
};

/// Certificate validity **[default: 20 years]**. Cosmetic — the client
/// ignores expiry.
const CERT_VALIDITY_YEARS: i32 = 20;
/// Mean Gregorian year, for turning the wall clock into a year number.
const SECONDS_PER_YEAR: u64 = 31_556_952;

pub(super) const KEY_FILE: &str = "identity.key";
const CERT_FILE: &str = "identity.pem";
pub(super) const RETIRED_DIR: &str = "retired";

/// A key under `retired/`: when it was retired and whether it still
/// vouches for the current key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredKey {
    pub fingerprint: Fingerprint,
    /// Seconds since the Unix epoch.
    pub retired_at: u64,
    /// The time is the file's mtime, not a recorded one: builds before
    /// 1.2.1 named the file `<stem>.key`, and their rename kept the key's
    /// creation time there (review P1-15).
    pub from_mtime: bool,
    /// Within [`RETIRED_KEY_RETENTION`] of `retired_at`.
    pub vouching: bool,
}

impl RetiredKey {
    /// When it stops vouching, in seconds since the Unix epoch.
    #[must_use]
    pub fn vouches_until(&self) -> u64 {
        self.retired_at
            .saturating_add(RETIRED_KEY_RETENTION.as_secs())
    }
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

        // Retention counts from the retirement, so record it: a rename
        // keeps the key's creation time as the mtime, and a key retired
        // after two years never vouched (review P1-15). The name survives
        // copies and restores; the mtime is set too for an older build.
        let now = SystemTime::now();
        let retired_path = retired_dir.join(format!(
            "{}.{}.key",
            current.fingerprint.file_stem(),
            unix_secs(now)
        ));
        fs::rename(&key_path, &retired_path)
            .map_err(|e| IdentityError::Io(retired_path.clone(), e))?;
        if let Err(e) = fs::File::options()
            .write(true)
            .open(&retired_path)
            .and_then(|f| f.set_modified(now))
        {
            warn!("Could not set the date of {}: {e}", retired_path.display());
        }
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

    #[cfg(test)]
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

    /// Every readable key under `<dir>/retired/`, newest retirement
    /// first, vouching or not: what `rotate-key` reports.
    #[must_use]
    pub fn retired_keys(dir: &Path) -> Vec<RetiredKey> {
        read_retired(&dir.join(RETIRED_DIR), SystemTime::now())
            .into_iter()
            .map(|(info, _)| info)
            .collect()
    }

    /// The rustls server configuration: TLS 1.3 only, our ALPN, the key
    /// log only when asked ([`super::key_log`]).
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
        config.key_log = super::key_log();
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

/// The retired keys still within retention.
fn load_retired(dir: &Path) -> Vec<KeyPair> {
    read_retired(dir, SystemTime::now())
        .into_iter()
        .filter_map(|(info, key)| {
            if !info.vouching {
                info!(
                    "Retired key {} was retired more than the retention window ago; no longer \
                     vouching",
                    info.fingerprint
                );
            }
            info.vouching.then_some(key)
        })
        .collect()
}

/// Every readable `*.key` in `dir` with its retirement time, newest
/// first, one entry per key.
fn read_retired(dir: &Path, now: SystemTime) -> Vec<(RetiredKey, KeyPair)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("key") {
            continue;
        }
        let recorded = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(retired_at_from_name);
        let (retired_at, from_mtime) = recorded.map_or_else(
            || {
                let mtime = entry.metadata().and_then(|m| m.modified()).ok();
                (mtime.map_or_else(|| unix_secs(now), unix_secs), true)
            },
            |secs| (secs, false),
        );
        let key = match load_key(&path) {
            Ok(key) => key,
            Err(e) => {
                warn!("Skipping unreadable retired key: {e}");
                continue;
            }
        };
        let age = unix_secs(now).saturating_sub(retired_at);
        let info = RetiredKey {
            fingerprint: Fingerprint::of_spki(&key.subject_public_key_info()),
            retired_at,
            from_mtime,
            vouching: age <= RETIRED_KEY_RETENTION.as_secs(),
        };
        keys.push((info, key));
    }
    // A key retired twice (an old identity restored, then rotated again)
    // vouches once, from its latest retirement.
    keys.sort_by_key(|(info, _)| std::cmp::Reverse(info.retired_at));
    let mut seen = std::collections::HashSet::new();
    keys.retain(|(info, _)| seen.insert(info.fingerprint));
    keys
}

/// The retirement time in `<stem>.<unix-secs>.key`; `None` for an older
/// build's `<stem>.key` (the stem is URL-safe base64, never a dot).
pub(super) fn retired_at_from_name(name: &str) -> Option<u64> {
    let (_, secs) = name.strip_suffix(".key")?.rsplit_once('.')?;
    secs.parse().ok()
}

pub(super) fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
