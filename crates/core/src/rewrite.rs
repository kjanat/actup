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

/// A `# <ref> [note]` comment after a SHA pin that names the ref it was pinned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinMarker<'a> {
    /// The ref the comment names, like `v4`.
    pub tag: &'a str,
    /// The text after the ref, trimmed.
    pub note: &'a str,
}

impl<'a> PinMarker<'a> {
    /// Parse `tail`, the text after a ref, as a pin marker.
    ///
    /// The first token after `#` must be a semver version, `main`, `master`,
    /// `develop`, `trunk`, or a ref holding `/`, and holds no control character.
    #[must_use]
    pub fn parse(tail: &'a str) -> Option<Self> {
        let body = tail.trim_start().strip_prefix('#')?.trim();
        let (tag, note) = body
            .split_once(char::is_whitespace)
            .map_or((body, ""), |(tag, note)| (tag, note.trim()));
        if tag.is_empty() || tag.chars().any(char::is_control) {
            return None;
        }
        let names_ref = match version::Version::parse(tag) {
            version::Version::Semver { .. } => true,
            version::Version::Sha { .. } => false,
            version::Version::Branch { .. } | version::Version::Other { .. } => {
                matches!(tag, "main" | "master" | "develop" | "trunk") || tag.contains('/')
            }
        };
        names_ref.then_some(Self { tag, note })
    }

    /// The marker after the SHA pin `sha` when `versions` holds its tag on that commit.
    #[must_use]
    pub fn unpinnable(sha: &str, tail: &'a str, versions: &version::Set) -> Option<Self> {
        Self::parse(tail).filter(|marker| versions.tag_sha(marker.tag) == Some(sha))
    }
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

    /// Unpin to the tag `marker` names, keeping its note as the comment.
    #[must_use]
    pub fn unpin(marker: PinMarker<'_>) -> Self {
        Self {
            new_ref: marker.tag.to_string(),
            comment: (!marker.note.is_empty()).then(|| format!(" # {}", marker.note)),
            to_line_end: true,
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

    const fn marker<'a>(tag: &'a str, note: &'a str) -> PinMarker<'a> {
        PinMarker { tag, note }
    }

    #[test]
    fn pin_marker_names_a_version_branch_or_slashed_ref() {
        assert_eq!(PinMarker::parse(" # v4"), Some(marker("v4", "")));
        assert_eq!(
            PinMarker::parse("#\tv4.1.0  keep me "),
            Some(marker("v4.1.0", "keep me"))
        );
        assert_eq!(PinMarker::parse("# main"), Some(marker("main", "")));
        assert_eq!(
            PinMarker::parse("# release/v1"),
            Some(marker("release/v1", ""))
        );
        assert_eq!(PinMarker::parse("# mine"), None);
        assert_eq!(PinMarker::parse("# "), None);
        assert_eq!(PinMarker::parse("v4"), None);
        assert_eq!(PinMarker::parse(&format!("# {}", "a".repeat(40))), None);
        assert_eq!(PinMarker::parse("# v4\u{7}"), None);
        assert_eq!(PinMarker::parse("# v\u{85}4"), None);
    }

    #[test]
    fn unpinnable_needs_the_tag_on_the_pinned_commit() {
        let at_a = std::sync::Arc::new(info("v4.0.0", 'a'));
        let major = std::sync::Arc::new(info("v4", 'b'));
        let versions = version::Set::build(
            vec![at_a, std::sync::Arc::clone(&major)],
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(4, major)]),
            None,
        );
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        assert_eq!(
            PinMarker::unpinnable(&a, "# v4.0.0", &versions),
            Some(marker("v4.0.0", ""))
        );
        assert_eq!(
            PinMarker::unpinnable(&b, "# v4 note", &versions),
            Some(marker("v4", "note"))
        );
        assert_eq!(PinMarker::unpinnable(&a, "# v4", &versions), None);
        assert_eq!(PinMarker::unpinnable(&a, "# v5.0.0", &versions), None);
        assert_eq!(PinMarker::unpinnable(&a, "# main", &versions), None);
        assert_eq!(PinMarker::unpinnable(&a, "# mine", &versions), None);
    }

    #[test]
    fn unpin_writes_the_tag_and_keeps_the_note() {
        assert_eq!(
            Rewrite::unpin(PinMarker {
                tag: "v4",
                note: ""
            }),
            rewrite("v4", None, true)
        );
        assert_eq!(
            Rewrite::unpin(PinMarker {
                tag: "v4",
                note: "keep me"
            }),
            rewrite("v4", Some(" # keep me"), true)
        );
    }

    #[test]
    fn text_joins_the_ref_and_its_comment() {
        assert_eq!(rewrite("abc", Some(" # v1"), false).text(), "abc # v1");
        assert_eq!(rewrite("v1", None, true).text(), "v1");
    }
}
