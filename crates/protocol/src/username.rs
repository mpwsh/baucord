//! Username sanitization. The name arrives in `Hello` and is cleaned on
//! the server before it is ever shown to anyone.

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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn short_names_pass_through() {
        assert_eq!(sanitize_username("alice"), "alice");
    }
}
