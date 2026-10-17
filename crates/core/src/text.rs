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

/// Replace with U+FFFD the first `:` of each line of `text` that starts with `::` after leading whitespace, and every `[` that follows `##`.
///
/// A GitHub Actions runner splits stdout and stderr into lines at CR and LF. It reads a line that starts with `::` as a workflow command, and a line that contains `##[` as a legacy one.
#[must_use]
pub fn neutralize_workflow_commands(text: &str) -> Cow<'_, str> {
    if !text.contains("::") && !text.contains("##[") {
        return Cow::Borrowed(text);
    }
    let mut neutralized = String::with_capacity(text.len());
    let mut line_start = true;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let opens = match c {
            ':' => line_start && chars.peek() == Some(&':'),
            '[' => neutralized.ends_with("##"),
            _ => false,
        };
        neutralized.push(if opens { '\u{fffd}' } else { c });
        line_start = matches!(c, '\n' | '\r') || (line_start && c.is_whitespace());
    }
    if neutralized == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(neutralized)
    }
}

fn sanitize(text: &str, reject: impl Fn(char) -> bool) -> Cow<'_, str> {
    match replace(text, reject) {
        Cow::Borrowed(text) => neutralize_workflow_commands(text),
        Cow::Owned(text) => Cow::Owned(neutralize_workflow_commands(&text).into_owned()),
    }
}

/// Replace C0 and C1 controls other than tab and line feed, DEL, zero-width and bidi control characters, and the BOM in `text` with U+FFFD, and apply [`neutralize_workflow_commands`].
#[must_use]
pub fn sanitize_for_terminal(text: &str) -> Cow<'_, str> {
    sanitize(text, is_unsafe)
}

/// Replace what [`sanitize_for_terminal`] replaces, and line feeds, in a single-line field such as a path or a `uses:` value.
#[must_use]
pub fn sanitize_line(text: &str) -> Cow<'_, str> {
    sanitize(text, |c| c == '\n' || is_unsafe(c))
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
    fn workflow_commands_lose_their_leading_colon_on_every_line() {
        assert_eq!(
            neutralize_workflow_commands("::error::x\nok :: here\n  ::add-mask::y\n\t::z"),
            "\u{fffd}:error::x\nok :: here\n  \u{fffd}:add-mask::y\n\t\u{fffd}:z"
        );
        assert!(matches!(
            neutralize_workflow_commands("a::b\nc: :d"),
            Cow::Borrowed("a::b\nc: :d")
        ));
        assert_eq!(
            sanitize_line("::error file=x::forged.yml"),
            "\u{fffd}:error file=x::forged.yml"
        );
        assert_eq!(
            sanitize_for_terminal("bad\u{202e}\n::stop-commands::t"),
            "bad\u{fffd}\n\u{fffd}:stop-commands::t"
        );
        assert_eq!(sanitize_line("x\n::error::y"), "x\u{fffd}::error::y");
    }

    #[test]
    fn workflow_commands_open_after_cr_and_legacy_ones_anywhere() {
        assert_eq!(
            neutralize_workflow_commands("v\r::error::cr\r\n ::x"),
            "v\r\u{fffd}:error::cr\r\n \u{fffd}:x"
        );
        assert_eq!(
            neutralize_workflow_commands("o/r@##[error]x ###[warning]y ##z [a]"),
            "o/r@##\u{fffd}error]x ###\u{fffd}warning]y ##z [a]"
        );
        assert_eq!(
            sanitize_line("##[error]forged.yml"),
            "##\u{fffd}error]forged.yml"
        );
        assert!(matches!(
            neutralize_workflow_commands("a:: b #[c] ::"),
            Cow::Borrowed("a:: b #[c] ::")
        ));
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
