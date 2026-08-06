//! Username handling and the netcode `user_data` encoding.
//!
//! Used by all three parties: the client encodes its name in open
//! (unsecure) mode, the token issuer embeds the name inside secure
//! connect tokens, and the server decodes + sanitizes whichever path the
//! name arrived by.

/// Size of the netcode user data blob. Must equal
/// `renet_netcode::NETCODE_USER_DATA_BYTES`; the binaries assert this at
/// compile time (this crate deliberately doesn't depend on the transport).
pub const USER_DATA_BYTES: usize = 256;

/// Maximum username length in bytes after sanitization.
pub const MAX_USERNAME_BYTES: usize = 32;

/// Sanitize an untrusted username: trim whitespace, strip control
/// characters, and cap at [`MAX_USERNAME_BYTES`] on a char boundary.
///
/// May return an empty string — callers substitute a placeholder
/// (the server uses `User<id>`).
#[must_use]
pub fn sanitize_username(raw: &str) -> String {
    let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    truncate_on_char_boundary(&cleaned, MAX_USERNAME_BYTES)
}

/// Truncate a string to at most `max_bytes` without splitting a UTF-8
/// character.
#[must_use]
pub fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Encode a (pre-sanitized) username into a netcode user data blob:
/// `[len: u8][utf-8 bytes][zero padding]`.
#[must_use]
pub fn encode_username(username: &str) -> [u8; USER_DATA_BYTES] {
    let name = truncate_on_char_boundary(username, MAX_USERNAME_BYTES);
    let mut data = [0u8; USER_DATA_BYTES];
    // MAX_USERNAME_BYTES < 256, so the length always fits in one byte.
    data[0] = name.len() as u8;
    data[1..1 + name.len()].copy_from_slice(name.as_bytes());
    data
}

/// Decode a username from a netcode user data blob. Invalid lengths or
/// non-UTF-8 content yield an empty string (callers substitute a
/// placeholder after [`sanitize_username`]).
#[must_use]
pub fn decode_username(user_data: &[u8; USER_DATA_BYTES]) -> String {
    let len = user_data[0] as usize;
    if len == 0 || len > MAX_USERNAME_BYTES {
        return String::new();
    }
    match std::str::from_utf8(&user_data[1..1 + len]) {
        Ok(name) => name.to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = encode_username("alice");
        assert_eq!(decode_username(&data), "alice");
    }

    #[test]
    fn sanitize_strips_controls_and_trims() {
        assert_eq!(sanitize_username("  bob\x00\x1b[31m  "), "bob[31m");
        assert_eq!(sanitize_username("\n\t"), "");
    }

    #[test]
    fn long_names_capped_on_char_boundary() {
        let name = "é".repeat(40); // 2 bytes per char
        let s = sanitize_username(&name);
        assert!(s.len() <= MAX_USERNAME_BYTES);
        assert_eq!(s, "é".repeat(16));

        let data = encode_username(&name);
        let decoded = decode_username(&data);
        assert_eq!(decoded, "é".repeat(16));
    }

    #[test]
    fn bad_length_yields_empty() {
        let mut data = [0u8; USER_DATA_BYTES];
        data[0] = 200; // > MAX_USERNAME_BYTES
        assert_eq!(decode_username(&data), "");
    }

    #[test]
    fn invalid_utf8_yields_empty() {
        let mut data = [0u8; USER_DATA_BYTES];
        data[0] = 2;
        data[1] = 0xFF;
        data[2] = 0xFE;
        assert_eq!(decode_username(&data), "");
    }
}
