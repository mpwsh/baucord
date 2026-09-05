//! Invite codes (spec §3.1): the parts both ends must agree on. Minting
//! a code needs randomness and lives on the server; what a code *looks
//! like* and how a person's typing is canonicalized is shared, so the
//! client can tidy input and the server can trust it.
//!
//! A newcomer gets three things, all plain text: the server's address,
//! its port, and a code. There is no link scheme to register — the app
//! is portable and stays that way.

/// Codes are 12 characters from an alphabet without `0/O/1/l/I`, so
/// they survive being read out loud on voice. 31¹² ≈ 2⁵⁹.
pub const CODE_LEN: usize = 12;
pub const ALPHABET: &[u8; 31] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Canonical form of a code as a person typed or read it: uppercase,
/// separators dropped. `None` if it cannot be a code at all.
#[must_use]
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if code.len() != CODE_LEN || !code.bytes().all(|b| ALPHABET.contains(&b)) {
        return None;
    }
    Some(code)
}

/// The code as people read and type it: `K7FQ-2MX9-ABCD`. Groups of
/// four are easy to read out; the dashes are dropped again by
/// [`normalize_code`].
#[must_use]
pub fn display_code(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + code.len() / 4);
    for (i, c) in code.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_normalize_and_reject_the_ambiguous_characters() {
        assert_eq!(
            normalize_code("k7fq-2mx9 abcd").as_deref(),
            Some("K7FQ2MX9ABCD")
        );
        assert_eq!(
            normalize_code("K7FQ2MX9ABC0"),
            None,
            "0 is not in the alphabet"
        );
        assert_eq!(normalize_code("K7FQ2MX9ABC"), None, "too short");
        assert_eq!(normalize_code(""), None);
    }

    #[test]
    fn display_form_groups_by_four_and_normalizes_back() {
        assert_eq!(display_code("K7FQ2MX9ABCD"), "K7FQ-2MX9-ABCD");
        assert_eq!(
            normalize_code("K7FQ-2MX9-ABCD").as_deref(),
            Some("K7FQ2MX9ABCD")
        );
        assert_eq!(display_code(""), "");
    }
}
