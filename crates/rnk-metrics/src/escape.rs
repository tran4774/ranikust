//! Text on its way to the operator's terminal or CI log.
//!
//! Messages carry names ranikust does not choose: tar entry names from a
//! base image, build-context names, registry text. A terminal acts on some
//! characters in them (an OSC sequence sets the title or the clipboard, a
//! line feed starts a line that looks like ranikust's own), so everything
//! ranikust writes to stderr itself, log lines and the fatal error line,
//! goes through here. Library errors keep the names as they are: their
//! `Display` is also read by programs, and only the sink knows that the
//! text is about to reach a terminal.

use std::borrow::Cow;

/// Whether a terminal or a log viewer could act on `c`: a control character
/// (C0 with line feed, carriage return and tab, DEL, C1), the Unicode line
/// and paragraph separators (U+2028, U+2029: a line break to some viewers),
/// or a bidi control, which reorders what is shown.
pub(crate) fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{61c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// `text` with every character [`needs_escape`] names written as Rust
/// writes it in a string literal (`\n`, `\t`, `\u{1b}`), so the text stays
/// on one line and shows what it holds. Text with nothing to escape is
/// returned as it is.
///
/// A backslash is not escaped: `\n` in the output is either an escaped
/// line feed or the two characters of a name, never a line break.
#[must_use]
pub fn terminal_text(text: &str) -> Cow<'_, str> {
    if !text.chars().any(needs_escape) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if needs_escape(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Serialized JSON with DEL, C1, the bidi controls and U+2028/U+2029
/// written as `\uXXXX`.
/// `serde_json` escapes only what JSON requires (below U+0020); those
/// characters can only occur inside strings, so the value read back is the
/// same.
pub(crate) fn json_text(json: String) -> String {
    if !json.chars().any(needs_escape) {
        return json;
    }
    let mut out = String::with_capacity(json.len() + 8);
    for c in json.chars() {
        if needs_escape(c) {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_is_borrowed_as_it_is() {
        let s = "tar entry usr/lib/ünïcode-名前 \\n: refused";
        assert!(matches!(terminal_text(s), Cow::Borrowed(t) if t == s));
    }

    #[test]
    fn every_control_and_bidi_character_is_escaped() {
        let cases = [
            ("\x1b]52;c;eA==\x07", r"\u{1b}]52;c;eA==\u{7}"),
            ("a\r\nb\tc", r"a\r\nb\tc"),
            ("\x00\x7f", r"\u{0}\u{7f}"),
            ("\u{9b}31m", r"\u{9b}31m"),
            ("a\u{2028}b\u{2029}c", r"a\u{2028}b\u{2029}c"),
            (
                "\u{202e}\u{2066}\u{61c}\u{200f}",
                r"\u{202e}\u{2066}\u{61c}\u{200f}",
            ),
        ];
        for (raw, shown) in cases {
            assert_eq!(terminal_text(raw), shown, "{raw:?}");
        }
    }

    #[test]
    fn json_keeps_its_value() {
        let value = "a\u{7f}\u{85}\u{9b}\u{202e}b\n\u{2028}\u{2029}";
        let line = json_text(serde_json::Value::String(value.to_owned()).to_string());
        assert_eq!(line, r#""a\u007f\u0085\u009b\u202eb\n\u2028\u2029""#);
        assert_eq!(serde_json::from_str::<String>(&line).unwrap(), value);
    }
}
