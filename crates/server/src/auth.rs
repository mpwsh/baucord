//! Netcode key management and the connect-token issuer.
//!
//! ## How secure mode works
//!
//! netcode's `Secure` authentication is a bearer-token scheme: a trusted
//! issuer holding the server's 32-byte private key mints short-lived
//! `ConnectToken`s; the UDP handshake only accepts clients presenting a
//! valid token. The token also fixes the client's id and username — the
//! client no longer chooses either, which is what makes media
//! attribution trustworthy (see `protocol::relayed`).
//!
//! The issuer here is a tiny synchronous HTTP endpoint on its own
//! thread, inside the server process — so the private key never leaves
//! this process and there is no key-distribution problem.
//!
//! ## Deployment
//!
//! The issuer speaks **plain HTTP** and binds loopback by default. A
//! connect token's public portion contains the session keys in the
//! clear (by design — the client needs them), so the hop between client
//! and issuer **must** be TLS. Terminate TLS in the reverse proxy
//! already fronting the deployment (APISIX in ours) and route e.g.
//! `POST https://yourhost/token` → `http://127.0.0.1:5001/token`.
//!
//! ## Endpoints
//!
//! - `POST /token` — body: desired username (UTF-8, ≤ 512 bytes read).
//!   Optional `x-baucord-pass` header if a shared password is
//!   configured. Response: `application/octet-stream` connect token
//!   bytes, ready for `ConnectToken::read`.
//! - `GET /healthz` — liveness probe, returns `ok`.

use std::{io::Read, net::SocketAddr, path::Path, time::SystemTime};

use log::{info, warn};
use renet_netcode::{generate_random_bytes, ConnectToken, NETCODE_KEY_BYTES};
use tiny_http::{Header, Method, Response, Server};

/// Wall-clock lifetime of an issued token. Tokens are single-use in
/// practice (fetched immediately before connecting), so keep this short.
const TOKEN_EXPIRE_SECS: u64 = 300;

/// netcode connection timeout embedded in the token (seconds without
/// traffic before the server drops the client).
const TOKEN_TIMEOUT_SECS: i32 = 15;

/// Maximum bytes of request body we will read for a username.
const MAX_USERNAME_BODY_BYTES: u64 = 512;

/// Load the netcode private key from `path`, or generate and persist a
/// fresh one if the file doesn't exist.
///
/// Format: 64 lowercase hex characters (32 bytes). Generated files are
/// created with owner-only permissions on Unix.
pub fn load_or_create_private_key(
    path: &str,
) -> Result<[u8; NETCODE_KEY_BYTES], Box<dyn std::error::Error>> {
    if Path::new(path).exists() {
        let contents = std::fs::read_to_string(path)?;
        let key = parse_hex_key(contents.trim())
            .ok_or_else(|| format!("{path} does not contain a valid 64-hex-char key"))?;
        info!("Loaded netcode private key from {path}");
        return Ok(key);
    }

    let key: [u8; NETCODE_KEY_BYTES] = generate_random_bytes();
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(path, &hex)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    info!("Generated new netcode private key at {path}");
    Ok(key)
}

fn parse_hex_key(s: &str) -> Option<[u8; NETCODE_KEY_BYTES]> {
    if s.len() != NETCODE_KEY_BYTES * 2 || !s.is_ascii() {
        return None;
    }
    let mut key = [0u8; NETCODE_KEY_BYTES];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        key[i] = ((hi << 4) | lo) as u8;
    }
    Some(key)
}

/// Bind the issuer's HTTP listener and serve it on a background thread.
///
/// Binding happens on the caller's thread so a bad `bind` address fails
/// server startup loudly instead of dying silently later.
pub fn spawn_issuer(
    bind: &str,
    password: Option<String>,
    private_key: [u8; NETCODE_KEY_BYTES],
    public_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let http =
        Server::http(bind).map_err(|e| format!("token issuer failed to bind {bind}: {e}"))?;
    info!("Token issuer listening on http://{bind} (POST /token) — put TLS in front");

    std::thread::Builder::new()
        .name("token-issuer".to_string())
        .spawn(move || issuer_loop(&http, password.as_deref(), &private_key, public_addr))?;
    Ok(())
}

fn issuer_loop(
    http: &Server,
    password: Option<&str>,
    private_key: &[u8; NETCODE_KEY_BYTES],
    public_addr: SocketAddr,
) {
    // Static bytes: infallible parse.
    let octet_stream = Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..])
        .expect("static header");

    for mut request in http.incoming_requests() {
        let url = request.url().to_string();
        let method = request.method().clone();

        match (method, url.as_str()) {
            (Method::Get, "/healthz") => {
                let _ = request.respond(Response::from_string("ok"));
            }
            (Method::Post, "/token") => {
                // Protocol gate. In token mode netcode can't reject an
                // old client (it uses whatever protocol id we put in the
                // token), so this is the door: no header or a different
                // id ⇒ 426 with a user-facing body. Old clients show a
                // non-403 body in their connect error, so they get the
                // "download the latest version" text without an update.
                let presented = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv(protocol::PROTOCOL_HEADER))
                    .and_then(|h| h.value.as_str().trim().parse::<u64>().ok());
                if presented != Some(protocol::PROTOCOL_ID) {
                    warn!(
                        "Token request refused: client protocol {} (server {})",
                        presented.map_or_else(|| "missing".to_string(), |p| p.to_string()),
                        protocol::PROTOCOL_ID
                    );
                    let _ = request.respond(
                        Response::from_string(protocol::OUTDATED_MESSAGE)
                            .with_status_code(protocol::OUTDATED_STATUS),
                    );
                    continue;
                }

                // Shared-password gate, if configured.
                if let Some(expected) = password {
                    let presented = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("x-baucord-pass"))
                        .map(|h| h.value.as_str().to_string());
                    if presented.as_deref() != Some(expected) {
                        warn!("Token request refused: bad or missing password");
                        let _ = request
                            .respond(Response::from_string("forbidden").with_status_code(403));
                        continue;
                    }
                }

                let mut body = String::new();
                if request
                    .as_reader()
                    .take(MAX_USERNAME_BODY_BYTES)
                    .read_to_string(&mut body)
                    .is_err()
                {
                    let _ =
                        request.respond(Response::from_string("bad request").with_status_code(400));
                    continue;
                }

                match issue_token(&body, private_key, public_addr) {
                    Some((token_bytes, client_id, username)) => {
                        info!("Issued connect token for '{username}' (client id {client_id})");
                        let response =
                            Response::from_data(token_bytes).with_header(octet_stream.clone());
                        let _ = request.respond(response);
                    }
                    None => {
                        warn!("Failed to generate connect token");
                        let _ = request.respond(
                            Response::from_string("token generation failed").with_status_code(500),
                        );
                    }
                }
            }
            _ => {
                let _ = request.respond(Response::from_string("not found").with_status_code(404));
            }
        }
    }
}

/// Mint a token for `raw_username`. Returns the serialized token, the
/// assigned client id, and the sanitized username.
fn issue_token(
    raw_username: &str,
    private_key: &[u8; NETCODE_KEY_BYTES],
    public_addr: SocketAddr,
) -> Option<(Vec<u8>, u64, String)> {
    let username = protocol::sanitize_username(raw_username);

    // The issuer, not the client, assigns the id. Random 64-bit ids
    // make collisions vanishingly unlikely (and netcode refuses a
    // duplicate id at the handshake regardless).
    let mut client_id = u64::from_le_bytes(generate_random_bytes());
    if client_id == 0 {
        client_id = 1;
    }

    let user_data = protocol::encode_username(&username);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;

    let token = ConnectToken::generate(
        now,
        protocol::PROTOCOL_ID,
        TOKEN_EXPIRE_SECS,
        client_id,
        TOKEN_TIMEOUT_SECS,
        vec![public_addr],
        Some(&user_data),
        private_key,
    )
    .ok()?;

    let mut bytes = Vec::with_capacity(2048);
    token.write(&mut bytes).ok()?;
    Some((bytes, client_id, username))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_key_roundtrip() {
        let key: [u8; NETCODE_KEY_BYTES] = [0xAB; NETCODE_KEY_BYTES];
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(parse_hex_key(&hex), Some(key));
    }

    #[test]
    fn bad_hex_rejected() {
        assert!(parse_hex_key("").is_none());
        assert!(parse_hex_key("zz").is_none());
        assert!(parse_hex_key(&"ff".repeat(31)).is_none());
        assert!(parse_hex_key(&"zz".repeat(32)).is_none());
    }

    #[test]
    fn issue_token_produces_bytes() {
        let key = [7u8; NETCODE_KEY_BYTES];
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let (bytes, client_id, username) = issue_token("alice", &key, addr).unwrap();
        assert!(!bytes.is_empty());
        assert_ne!(client_id, 0);
        assert_eq!(username, "alice");
    }
}
