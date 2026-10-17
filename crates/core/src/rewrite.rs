//! The text written for an updated `uses:` ref and the comment after it.

use crate::{policy::Style, version};

/// What follows a `uses:` ref on its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trailing {
    /// Nothing but whitespace.
    None,
    /// A version comment like `# v8.1.0`.
    Version,
    /// Any other text.
    Foreign,
}

impl Trailing {
    /// Classify the text of `raw_line` after byte offset `ref_end`.
    #[must_use]
    pub fn after(raw_line: &str, ref_end: usize) -> Self {
        let tail = raw_line.get(ref_end..).unwrap_or("").trim_start();
        if tail.is_empty() {
            Self::None
        } else if is_version_comment(tail) {
            Self::Version
        } else {
            Self::Foreign
        }
    }
}

/// Whether a line fragment is a version comment like `# v1.2.3` or `# 1.2`.
#[must_use]
pub fn is_version_comment(tail: &str) -> bool {
    let Some(body) = tail.strip_prefix('#') else {
        return false;
    };
    let body = body.trim();
    let body = body.strip_prefix('v').unwrap_or(body);
    body.starts_with(|c: char| c.is_ascii_digit())
}

/// The replacement for a ref, starting at the ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    /// The ref after `@` as written.
    pub new_ref: String,
    /// The comment written right after the ref, like ` # v4.2.0`.
    pub comment: Option<String>,
    /// Whether the replacement runs to the end of the line, over the text after the ref.
    pub to_line_end: bool,
}

impl Rewrite {
    /// Write `target` for an outdated ref under `style`.
    ///
    /// A SHA gets a ` # <version>` comment unless foreign text follows the ref.
    /// A SHA pin rewritten to a tag loses its trailing version comment.
    #[must_use]
    pub fn outdated(
        style: Style,
        current_is_sha: bool,
        target: &version::Info,
        trailing: Trailing,
    ) -> Self {
        let version = target.version().to_string();
        if style.writes_sha(current_is_sha) {
            Self::pin(target.sha(), &version, trailing)
        } else {
            Self {
                new_ref: version,
                comment: None,
                to_line_end: current_is_sha && trailing == Trailing::Version,
            }
        }
    }

    /// Pin to `sha` with a ` # <label>` comment.
    ///
    /// A trailing version comment is replaced, a missing one is added, and
    /// foreign text after the ref is kept without a comment.
    #[must_use]
    pub fn pin(sha: &str, label: &str, trailing: Trailing) -> Self {
        Self {
            new_ref: sha.to_string(),
            comment: (trailing != Trailing::Foreign).then(|| format!(" # {label}")),
            to_line_end: trailing == Trailing::Version,
        }
    }

    /// The ref followed by its comment.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}{}", self.new_ref, self.comment.as_deref().unwrap_or(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str, sha: char) -> version::Info {
        version::Info::new(version::Version::parse(version), sha.to_string().repeat(40))
    }

    fn rewrite(new_ref: &str, comment: Option<&str>, to_line_end: bool) -> Rewrite {
        Rewrite {
            new_ref: new_ref.to_string(),
            comment: comment.map(str::to_string),
            to_line_end,
        }
    }

    fn trailing(line: &str) -> Trailing {
        let ref_end = line.find("@v1").map_or(0, |at| at + 3);
        Trailing::after(line, ref_end)
    }

    #[test]
    fn trailing_classifies_the_text_after_the_ref() {
        assert_eq!(trailing("  - uses: a/b@v1   # v1.2.0"), Trailing::Version);
        assert_eq!(trailing("  - uses: a/b@v1  "), Trailing::None);
        assert_eq!(trailing("  - { uses: a/b@v1 }"), Trailing::Foreign);
        assert_eq!(trailing("  - uses: a/b@v1 # mine"), Trailing::Foreign);
        assert_eq!(Trailing::after("  - uses: a/b@v1", 99), Trailing::None);
    }

    #[test]
    fn version_comment_needs_a_leading_digit() {
        assert!(is_version_comment("# v1.2.3"));
        assert!(is_version_comment("#1.2"));
        assert!(!is_version_comment("# main"));
        assert!(!is_version_comment("v1.2.3"));
    }

    #[test]
    fn outdated_follows_the_pin_style() {
        let target = info("v4.2.0", 'b');
        let sha = "b".repeat(40);
        let cases = [
            (
                Style::Preserve,
                false,
                Trailing::None,
                rewrite("v4.2.0", None, false),
            ),
            (
                Style::Preserve,
                true,
                Trailing::None,
                rewrite(&sha, Some(" # v4.2.0"), false),
            ),
            (
                Style::Preserve,
                true,
                Trailing::Version,
                rewrite(&sha, Some(" # v4.2.0"), true),
            ),
            (
                Style::Preserve,
                true,
                Trailing::Foreign,
                rewrite(&sha, None, false),
            ),
            (
                Style::Sha,
                false,
                Trailing::None,
                rewrite(&sha, Some(" # v4.2.0"), false),
            ),
            (
                Style::Sha,
                false,
                Trailing::Foreign,
                rewrite(&sha, None, false),
            ),
            (
                Style::Tag,
                false,
                Trailing::Version,
                rewrite("v4.2.0", None, false),
            ),
            (
                Style::Tag,
                true,
                Trailing::None,
                rewrite("v4.2.0", None, false),
            ),
            (
                Style::Tag,
                true,
                Trailing::Version,
                rewrite("v4.2.0", None, true),
            ),
            (
                Style::Tag,
                true,
                Trailing::Foreign,
                rewrite("v4.2.0", None, false),
            ),
        ];
        for (style, is_sha, trailing, expected) in cases {
            assert_eq!(
                Rewrite::outdated(style, is_sha, &target, trailing),
                expected,
                "{style} {is_sha} {trailing:?}"
            );
        }
    }

    #[test]
    fn text_joins_the_ref_and_its_comment() {
        assert_eq!(rewrite("abc", Some(" # v1"), false).text(), "abc # v1");
        assert_eq!(rewrite("v1", None, true).text(), "v1");
    }
}
