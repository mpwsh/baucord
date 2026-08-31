//! WhatsApp-flavored message markup → CommonMark for Slint's StyledText.
//!
//! Users write `*bold*`, `_italic_`, `` `inline code` `` and
//! ```` ``` ````-fenced code blocks (WhatsApp/Discord habits). Slint's
//! `StyledText` consumes CommonMark, whose semantics differ (`*x*` is
//! italic there) and whose 1.17 parser rejects several block
//! constructs outright (fenced code, headings, block quotes) — a
//! message like "# yolo" would fail to parse entirely. So:
//!
//! * [`split_fences`] pulls ```` ``` ````-fenced blocks out first; the
//!   UI renders those as dedicated monospace rows (the same trick the
//!   history already plays with links).
//! * [`to_commonmark`] converts a text segment: our tokens are
//!   re-emitted as CommonMark (`*b*` → `**b**`, `_i_` → `*i*`, code
//!   spans verbatim), every other Markdown-active character is
//!   backslash-escaped, and newlines become hard breaks. The result
//!   always parses, and never styles anything the user didn't ask for.

/// One piece of a message after fence extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Regular text (still carrying `*`/`_`/`` ` `` markup).
    Text(String),
    /// The verbatim inside of a ```` ``` ```` fence.
    Code(String),
}

/// Split a message on ```` ``` ```` delimiters, Discord-style: a pair
/// of triple-backticks makes a code block whether it spans lines
/// (```` ```\ncode\n``` ````) or sits inline (```` ```code``` ````).
/// An optional language word right after the opening fence is dropped
/// when the block is multi-line. An unterminated ```` ``` ```` is not
/// a fence — the rest stays literal text, like WhatsApp treats
/// unpaired markup.
#[must_use]
pub fn split_fences(body: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = body;
    loop {
        let Some(open) = rest.find("```") else {
            text.push_str(rest);
            break;
        };
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else {
            text.push_str(rest);
            break;
        };
        text.push_str(&rest[..open]);
        // The newline that separates surrounding text from a fence
        // belongs to the fence, not the text.
        if text.ends_with('\n') {
            text.pop();
        }
        let mut code = &after[..close];
        // Multi-line block: drop an optional info string ("rust") on
        // the opening line, then the fence's own line breaks.
        if let Some(nl) = code.find('\n') {
            let info = code[..nl].trim();
            if !info.is_empty()
                && info
                    .chars()
                    .all(|c| c.is_alphanumeric() || "+-_#.".contains(c))
            {
                code = &code[nl + 1..];
            }
        }
        let code = code.strip_prefix('\n').unwrap_or(code);
        let code = code.strip_suffix('\n').unwrap_or(code);
        if !text.trim().is_empty() {
            out.push(Segment::Text(std::mem::take(&mut text)));
        } else {
            text.clear();
        }
        out.push(Segment::Code(code.to_string()));
        rest = &after[close + 3..];
        rest = rest.strip_prefix('\n').unwrap_or(rest);
    }
    if !text.trim().is_empty() || out.is_empty() {
        out.push(Segment::Text(text));
    }
    out
}

/// True when `c` starts/ends an emphasis run: flanked by whitespace or
/// segment edges on the outside, non-whitespace on the inside — the
/// WhatsApp rule, so `snake_case_names` and `2*3*4` stay literal.
fn can_open(chars: &[char], i: usize) -> bool {
    let before_ok = i == 0 || chars[i - 1].is_whitespace();
    let after_ok = i + 1 < chars.len() && !chars[i + 1].is_whitespace() && chars[i + 1] != chars[i];
    before_ok && after_ok
}

fn can_close(chars: &[char], i: usize) -> bool {
    let after_ok = i + 1 == chars.len() || {
        let c = chars[i + 1];
        c.is_whitespace() || c.is_ascii_punctuation()
    };
    let before_ok = i > 0 && !chars[i - 1].is_whitespace() && chars[i - 1] != chars[i];
    before_ok && after_ok
}

/// Find the matching closer for the token at `open`, on the same line.
fn find_close(chars: &[char], open: usize, tok: char) -> Option<usize> {
    (open + 1..chars.len())
        .take_while(|&j| chars[j] != '\n')
        .find(|&j| chars[j] == tok && can_close(chars, j))
}

/// If `chars` starts with a URL, return the destination (with the
/// `https://` prefix supplied for bare `www.` links) and how many
/// chars the visible label spans. Mirrors the old link-row detector:
/// trailing sentence punctuation is not part of the link, and a bare
/// scheme with nothing behind it doesn't count.
fn match_url(chars: &[char]) -> Option<(String, usize)> {
    let piece: String = chars.iter().take_while(|c| !c.is_whitespace()).collect();
    let lower = piece.to_ascii_lowercase();
    let is_url = lower.starts_with("http://") || lower.starts_with("https://");
    let is_www = !is_url && lower.starts_with("www.") && piece.len() > 4;
    if !(is_url || is_www) {
        return None;
    }
    let trimmed = piece.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'']);
    if trimmed.len() < 8 && !is_www {
        return None; // "http://" with nothing behind it
    }
    let dest = if is_www {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    };
    Some((dest, trimmed.chars().count()))
}

/// Escape one char into CommonMark-inert output.
fn push_escaped(out: &mut String, c: char) {
    if c == '\n' {
        // Hard break: deterministic line break in StyledText.
        out.push('\\');
        out.push('\n');
    } else {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Convert one text segment to CommonMark. Returns the markup and
/// whether any styling was produced (callers skip the StyledText path
/// entirely for plain messages, keeping them selectable).
///
/// `code_color`: an optional `#RRGGBB` tint for inline code spans —
/// Slint's dialect accepts `<font color>` inline HTML, so a span
/// renders monospace AND tinted (the renderer offers no background,
/// so this is the closest to a Slack-style code chip).
#[must_use]
pub fn to_commonmark(text: &str, code_color: Option<&str>) -> (String, bool) {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut styled = false;
    let mut in_heading = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // `# text` at a line start renders as a bold line — StyledText
        // has no per-span sizes, so bold is the closest heading look.
        if c == '#' && (i == 0 || chars[i - 1] == '\n') && !in_heading {
            let mut level = 0;
            while i + level < chars.len() && chars[i + level] == '#' {
                level += 1;
            }
            if level <= 6 && i + level < chars.len() && chars[i + level] == ' ' {
                out.push_str("**");
                in_heading = true;
                styled = true;
                i += level + 1;
                continue;
            }
        }
        if in_heading && c == '\n' {
            out.push_str("**");
            in_heading = false;
        }
        match c {
            // Inline code span: verbatim between single backticks, no
            // newline inside. CommonMark code spans take content
            // literally, so nothing inside needs escaping (a backtick
            // can't appear inside a single-backtick span anyway).
            '`' => {
                if let Some(close) = (i + 1..chars.len())
                    .take_while(|&j| chars[j] != '\n')
                    .find(|&j| chars[j] == '`')
                {
                    if close > i + 1 {
                        if let Some(color) = code_color {
                            out.push_str("<font color=\"");
                            out.push_str(color);
                            out.push_str("\">`");
                            out.extend(&chars[i + 1..close]);
                            out.push_str("`</font>");
                        } else {
                            out.push('`');
                            out.extend(&chars[i + 1..close]);
                            out.push('`');
                        }
                        styled = true;
                        i = close + 1;
                        continue;
                    }
                }
                push_escaped(&mut out, c);
                i += 1;
            }
            '*' | '_' if can_open(&chars, i) => {
                if let Some(close) = find_close(&chars, i, c) {
                    let (open_mark, close_mark) = if c == '*' {
                        ("**", "**") // WhatsApp bold
                    } else {
                        ("*", "*") // WhatsApp italic
                    };
                    out.push_str(open_mark);
                    for &inner in &chars[i + 1..close] {
                        push_escaped(&mut out, inner);
                    }
                    out.push_str(close_mark);
                    styled = true;
                    i = close + 1;
                    continue;
                }
                push_escaped(&mut out, c);
                i += 1;
            }
            _ => {
                // Linkify URLs at word starts: emitted as CommonMark
                // links so StyledText renders them blue and clickable
                // inline (the old design appended a separate link row
                // under the message, duplicating the text).
                if (i == 0 || chars[i - 1].is_whitespace())
                    && let Some((url, label_len)) = match_url(&chars[i..])
                {
                    out.push('[');
                    for &lc in &chars[i..i + label_len] {
                        push_escaped(&mut out, lc);
                    }
                    out.push_str("](<");
                    out.push_str(&url);
                    out.push_str(">)");
                    styled = true;
                    i += label_len;
                    continue;
                }
                push_escaped(&mut out, c);
                i += 1;
            }
        }
    }
    if in_heading {
        out.push_str("**");
    }
    (out, styled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_commonmark_none(text: &str) -> (String, bool) {
        to_commonmark(text, None)
    }

    fn text(s: &str) -> Segment {
        Segment::Text(s.into())
    }

    fn code(s: &str) -> Segment {
        Segment::Code(s.into())
    }

    #[test]
    fn plain_text_is_one_segment() {
        assert_eq!(split_fences("hello there"), vec![text("hello there")]);
    }

    #[test]
    fn fenced_block_is_extracted() {
        assert_eq!(
            split_fences("look:\n```\nlet x = 1;\n```\nnice"),
            vec![text("look:"), code("let x = 1;"), text("nice")]
        );
    }

    #[test]
    fn fence_with_info_string_and_no_tail() {
        assert_eq!(
            split_fences("```rust\nfn f() {}\n```"),
            vec![code("fn f() {}")]
        );
    }

    #[test]
    fn unterminated_fence_stays_text() {
        assert_eq!(split_fences("uh ```\noops"), vec![text("uh ```\noops")]);
    }

    #[test]
    fn inline_fence_on_one_line() {
        assert_eq!(
            split_fences("before ```let x = 1;``` after"),
            vec![text("before "), code("let x = 1;"), text(" after")]
        );
    }

    #[test]
    fn heading_becomes_bold_line() {
        let (cm, styled) = to_commonmark_none("# Big news\nregular");
        assert!(styled);
        assert_eq!(cm, "**Big news**\\\nregular");
    }

    #[test]
    fn hash_without_space_stays_literal() {
        let (cm, styled) = to_commonmark_none("#hashtag");
        assert!(!styled);
        assert_eq!(cm, "\\#hashtag");
    }

    #[test]
    fn bold_and_italic_translate() {
        let (cm, styled) = to_commonmark_none("a *b* and _c_ end");
        assert_eq!(cm, "a **b** and *c* end");
        assert!(styled);
    }

    #[test]
    fn inline_code_passes_verbatim() {
        let (cm, styled) = to_commonmark_none("run `cargo *test*` now");
        assert_eq!(cm, "run `cargo *test*` now");
        assert!(styled);
    }

    #[test]
    fn inline_code_gets_the_tint() {
        let (cm, styled) = to_commonmark("run `x` now", Some("#86AA90"));
        assert_eq!(cm, "run <font color=\"#86AA90\">`x`</font> now");
        assert!(styled);
    }

    #[test]
    fn plain_message_is_not_styled() {
        let (_, styled) = to_commonmark_none("just words 2*3 and snake_case_name");
        assert!(!styled);
    }

    #[test]
    fn markdown_actives_are_escaped() {
        // (a leading `# ` would now legitimately style as a heading)
        let (cm, styled) = to_commonmark_none("mid #tag > nor quote [x](y)");
        assert!(!styled);
        assert_eq!(cm, "mid \\#tag \\> nor quote \\[x\\]\\(y\\)");
    }

    #[test]
    fn intraword_tokens_stay_literal() {
        let (cm, styled) = to_commonmark_none("snake_case and 2*3*4");
        assert!(!styled);
        assert_eq!(cm, "snake\\_case and 2\\*3\\*4");
    }

    #[test]
    fn newlines_become_hard_breaks() {
        let (cm, _) = to_commonmark_none("one\ntwo");
        assert_eq!(cm, "one\\\ntwo");
    }

    #[test]
    fn urls_become_inline_links() {
        let (cm, styled) = to_commonmark_none("see https://x.com/home now");
        assert!(styled);
        assert_eq!(
            cm,
            r"see [https\:\/\/x\.com\/home](<https://x.com/home>) now"
        );
    }

    #[test]
    fn url_trailing_punctuation_stays_out() {
        let (cm, _) = to_commonmark_none("go to www.example.com, ok?");
        assert_eq!(
            cm,
            r"go to [www\.example\.com](<https://www.example.com>)\, ok\?"
        );
    }

    #[test]
    fn url_in_code_span_stays_code() {
        let (cm, _) = to_commonmark_none("`https://x.com` raw");
        assert_eq!(cm, "`https://x.com` raw");
    }

    #[test]
    fn bare_scheme_is_not_a_link() {
        let (cm, styled) = to_commonmark_none("http:// nope");
        assert!(!styled);
        assert_eq!(cm, r"http\:\/\/ nope");
    }

    #[test]
    fn unpaired_tokens_stay_literal() {
        let (cm, styled) = to_commonmark_none("*dangling and _also");
        assert!(!styled);
        assert_eq!(cm, "\\*dangling and \\_also");
    }

    #[test]
    fn emphasis_does_not_cross_lines() {
        let (_, styled) = to_commonmark_none("*top\nbottom*");
        assert!(!styled);
    }
}
