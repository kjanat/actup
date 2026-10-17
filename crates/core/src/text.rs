//! Text safety for terminal and editor message output.

use std::borrow::Cow;

const fn is_unsafe(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

fn replace(text: &str, reject: impl Fn(char) -> bool) -> Cow<'_, str> {
    if text.contains(&reject) {
        Cow::Owned(
            text.chars()
                .map(|c| if reject(c) { '\u{fffd}' } else { c })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// Replace C0 and C1 controls other than tab and line feed, DEL, zero-width and bidi control characters, and the BOM in `text` with U+FFFD.
#[must_use]
pub fn sanitize_for_terminal(text: &str) -> Cow<'_, str> {
    replace(text, is_unsafe)
}

/// Replace what [`sanitize_for_terminal`] replaces, and line feeds, in a single-line field such as a path or a `uses:` value.
#[must_use]
pub fn sanitize_line(text: &str) -> Cow<'_, str> {
    replace(text, |c| c == '\n' || is_unsafe(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_every_unsafe_character() {
        let unsafe_chars = (0u32..=0x1f)
            .filter(|c| ![0x09, 0x0a].contains(c))
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
            "tab\tline\nfeed",
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
    fn sanitize_line_also_replaces_line_feeds() {
        assert_eq!(
            sanitize_line("o/r@v1\r  L2: x\n  L3\tok\u{202e}"),
            "o/r@v1\u{fffd}  L2: x\u{fffd}  L3\tok\u{fffd}"
        );
        assert_eq!(sanitize_for_terminal("a\r\nb"), "a\u{fffd}\nb");
        assert!(matches!(sanitize_line("a\tb"), Cow::Borrowed("a\tb")));
    }

    #[test]
    fn sanitize_neutralises_escape_sequences_and_bidi_overrides() {
        assert_eq!(
            sanitize_for_terminal("o/r@v1\u{1b}[2K\u{202e}1v"),
            "o/r@v1\u{fffd}[2K\u{fffd}1v"
        );
    }
}
