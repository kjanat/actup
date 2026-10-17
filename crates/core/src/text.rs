//! Text safety for terminal and editor message output.

use std::borrow::Cow;

const fn is_unsafe(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{8}'
            | '\u{b}'
            | '\u{c}'
            | '\u{e}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// Replace C0 and C1 controls other than tab, line feed and carriage return, DEL, zero-width and bidi control characters, and the BOM in `text` with U+FFFD.
#[must_use]
pub fn sanitize_for_terminal(text: &str) -> Cow<'_, str> {
    if text.contains(is_unsafe) {
        Cow::Owned(
            text.chars()
                .map(|c| if is_unsafe(c) { '\u{fffd}' } else { c })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_every_unsafe_character() {
        let unsafe_chars = (0u32..=0x1f)
            .filter(|c| ![0x09, 0x0a, 0x0d].contains(c))
            .chain(0x7f..=0x9f)
            .chain(0x200b..=0x200f)
            .chain(0x202a..=0x202e)
            .chain(0x2066..=0x2069)
            .chain([0xfeff])
            .filter_map(char::from_u32);
        for c in unsafe_chars {
            let text = format!("a{c}b");
            assert_eq!(
                sanitize_for_terminal(&text),
                "a\u{fffd}b",
                "U+{:04X}",
                u32::from(c)
            );
        }
    }

    #[test]
    fn sanitize_keeps_safe_text_borrowed() {
        for text in [
            "actions/checkout@v4",
            "tab\tline\ncarriage\r",
            "déploiement ✓ 日本",
            "\u{a0}\u{2010}\u{2070}",
        ] {
            assert!(
                matches!(sanitize_for_terminal(text), Cow::Borrowed(t) if t == text),
                "{text:?}"
            );
        }
    }

    #[test]
    fn sanitize_neutralises_escape_sequences_and_bidi_overrides() {
        assert_eq!(
            sanitize_for_terminal("o/r@v1\u{1b}[2K\u{202e}1v"),
            "o/r@v1\u{fffd}[2K\u{fffd}1v"
        );
    }
}
