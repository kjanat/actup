//! The text written for an updated `uses:` ref and the comment after it.

use crate::{policy::Style, version};

/// What follows a `uses:` ref on its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trailing<'a> {
    /// Nothing but a closing quote, flow `}`, `]` and `,`, and spaces.
    None,
    /// A comment whose first word [`Labels::names`] accepts, like `# v8.1.0 keep` or `# main`.
    Label {
        /// The text after the first word, trimmed, like `keep`.
        note: &'a str,
    },
    /// Another comment, or other text such as more flow keys and no comment.
    Foreign,
}

impl<'a> Trailing<'a> {
    /// Classify the text of `raw_line` after the ref that ends at byte offset `ref_end`.
    #[must_use]
    pub fn after(raw_line: &'a str, ref_end: usize, labels: &Labels<'_>) -> Self {
        Self::of(
            closer(raw_line, ref_end),
            after_value(raw_line, ref_end),
            labels,
        )
    }

    /// Classify a ref followed by `closer` and then `comment`, as [`closer`] and [`after_value`] split them.
    #[must_use]
    pub fn of(closer: &str, comment: &'a str, labels: &Labels<'_>) -> Self {
        if !comment.is_empty() {
            return match split_comment(comment) {
                Some((label, note)) if labels.names(label) => Self::Label { note },
                _ => Self::Foreign,
            };
        }
        let bare = closer
            .trim_start_matches(['"', '\''])
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, '}' | ']' | ','));
        if bare { Self::None } else { Self::Foreign }
    }
}

/// The refs that the first word of a comment after a ref may name.
///
/// A writer rewrites a comment whose first word is a version token like
/// `v4.2.0`, the ref it replaces, or a tag or branch that its fetched
/// version set lists. It keeps any other comment.
#[derive(Debug, Clone, Copy)]
pub struct Labels<'a> {
    current: &'a str,
    fetched: Fetched<'a>,
}

#[derive(Debug, Clone, Copy)]
enum Fetched<'a> {
    Nothing,
    Set(&'a version::Set),
    Checked(bool),
}

impl<'a> Labels<'a> {
    /// The labels for the ref `current` with no fetched refs.
    #[must_use]
    pub const fn unfetched(current: &'a str) -> Self {
        Self {
            current,
            fetched: Fetched::Nothing,
        }
    }

    /// The labels for the ref `current` and the tags and branches `versions` lists.
    #[must_use]
    pub const fn fetched(current: &'a str, versions: &'a version::Set) -> Self {
        Self {
            current,
            fetched: Fetched::Set(versions),
        }
    }

    /// The labels for the ref `current` and, when `listed` holds, the [`comment_label`] after it.
    ///
    /// `listed` records whether a version set listed that label when the ref was checked.
    #[must_use]
    pub const fn checked(current: &'a str, listed: bool) -> Self {
        Self {
            current,
            fetched: Fetched::Checked(listed),
        }
    }

    /// Whether a comment whose first word is `label` names the ref a writer replaces.
    #[must_use]
    pub fn names(&self, label: &str) -> bool {
        is_version_token(label)
            || label == self.current
            || match self.fetched {
                Fetched::Nothing => false,
                Fetched::Set(versions) => versions.names_ref(label),
                Fetched::Checked(listed) => listed,
            }
    }
}

/// The text between the ref that ends at byte offset `ref_end` and the comment of its line.
///
/// That is the rest of the value and its flow mapping, such as a closing
/// quote, `, with: { x: 1 } }` and spaces. A comment written after the ref
/// goes after this text.
#[must_use]
pub fn closer(raw_line: &str, ref_end: usize) -> &str {
    let rest = raw_line.get(ref_end..).unwrap_or("");
    &rest[..comment_start(rest)]
}

/// The comment of `raw_line` after the ref that ends at byte offset `ref_end`, or `""`.
#[must_use]
pub fn after_value(raw_line: &str, ref_end: usize) -> &str {
    let rest = raw_line.get(ref_end..).unwrap_or("");
    &rest[comment_start(rest)..]
}

/// The byte offset of the first `#` in `rest` that starts a YAML comment.
///
/// `rest` starts right after a ref, so a leading quote closes the value. A
/// later quote opens a quoted scalar only where a node starts: after `{`,
/// `[`, `,`, `:` or a `?` key indicator, and after the `!tag` and `&anchor`
/// properties of that node.
fn comment_start(rest: &str) -> usize {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut after_space = false;
    let mut node = false;
    let mut property = Property::None;
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match quote {
            _ if i == 0 && matches!(c, '"' | '\'') => {}
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some('\'') if c == '\'' && chars.next_if(|&(_, n)| n == '\'').is_some() => {}
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '#' && after_space => return i,
            None => {
                property = match property {
                    Property::Verbatim if c == '>' => Property::Token,
                    Property::Verbatim => Property::Verbatim,
                    Property::Token if !c.is_whitespace() && !is_flow_indicator(c) => {
                        Property::Token
                    }
                    Property::Token | Property::None if c.is_whitespace() => Property::None,
                    Property::Token | Property::None if node && matches!(c, '"' | '\'') => {
                        quote = Some(c);
                        node = false;
                        Property::None
                    }
                    Property::Token | Property::None if node && c == '!' => {
                        if chars.next_if(|&(_, n)| n == '<').is_some() {
                            Property::Verbatim
                        } else {
                            Property::Token
                        }
                    }
                    Property::Token | Property::None if node && c == '&' => Property::Token,
                    Property::Token | Property::None if node && c == '?' => Property::None,
                    Property::Token | Property::None => {
                        node = matches!(c, '{' | '[' | ',' | ':');
                        Property::None
                    }
                };
            }
        }
        after_space = c.is_whitespace();
    }
    rest.len()
}

#[derive(Clone, Copy)]
enum Property {
    None,
    Token,
    Verbatim,
}

const fn is_flow_indicator(c: char) -> bool {
    matches!(c, ',' | '{' | '}' | '[' | ']')
}

/// Whether a line fragment is a version comment like `# v1.2.3` or `# 1.2 note`.
///
/// The first word after `#` must be a version token as [`Labels::names`]
/// reads it.
#[must_use]
pub fn is_version_comment(tail: &str) -> bool {
    tail.starts_with('#') && split_comment(tail).is_some_and(|(label, _)| is_version_token(label))
}

/// The first word of the comment `tail`, like `v4` in `# v4 keep`.
#[must_use]
pub fn comment_label(tail: &str) -> Option<&str> {
    split_comment(tail)
        .map(|(label, _)| label)
        .filter(|label| !label.is_empty())
}

// Pre-release and build identifiers follow SemVer 2.0.0 §9 and §10.
fn is_version_token(token: &str) -> bool {
    let token = token.strip_prefix('v').unwrap_or(token);
    if version::is_iso_date(token) {
        return false;
    }
    let (token, build) = token
        .split_once('+')
        .map_or((token, None), |(token, build)| (token, Some(build)));
    let (numbers, prerelease) = token
        .split_once('-')
        .map_or((token, None), |(numbers, pre)| (numbers, Some(pre)));
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let is_identifier =
        |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let has_leading_zero =
        |id: &str| id.len() > 1 && id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit());
    (1..=3).contains(&numbers.split('.').count())
        && numbers.split('.').all(is_number)
        && prerelease.is_none_or(|pre| {
            pre.split('.')
                .all(|id| is_identifier(id) && !has_leading_zero(id))
        })
        && build.is_none_or(|build| build.split('.').all(is_identifier))
}

fn split_comment(tail: &str) -> Option<(&str, &str)> {
    let body = tail.trim_start().strip_prefix('#')?.trim();
    Some(
        body.split_once(char::is_whitespace)
            .map_or((body, ""), |(first, note)| (first, note.trim())),
    )
}

/// The comment ` # <label>`, followed by ` <note>` when `note` is not empty.
#[must_use]
pub fn labeled_comment(label: &str, note: &str) -> String {
    if note.is_empty() {
        format!(" # {label}")
    } else {
        format!(" # {label} {note}")
    }
}

/// The comment ` # <note>`, or `None` for an empty `note`.
#[must_use]
pub fn note_comment(note: &str) -> Option<String> {
    (!note.is_empty()).then(|| format!(" # {note}"))
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
        let (tag, note) = split_comment(tail)?;
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

/// Whether a ref written by name is a tag or a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// A tag, like `v4.2.0`.
    Tag,
    /// A branch, like `main`.
    Branch,
}

impl Named {
    /// A tag when `versions` lists a tag `name`, or lists no ref `name` and `name` is a version token, else a branch.
    #[must_use]
    pub fn listed(name: &str, versions: &version::Set) -> Self {
        let tagged = versions.has_exact_tag(name) || versions.other_tag_sha(name).is_some();
        if tagged || (!versions.names_ref(name) && is_version_token(name)) {
            Self::Tag
        } else {
            Self::Branch
        }
    }
}

impl Rewrite {
    /// Write the tag `target` for an outdated ref under `style`.
    ///
    /// A SHA is written by [`Rewrite::pin`] and the tag by [`Rewrite::named`].
    #[must_use]
    pub fn outdated(
        style: Style,
        current_is_sha: bool,
        target: &version::Info,
        trailing: Trailing<'_>,
    ) -> Self {
        let version = target.version().to_string();
        if style.writes_sha(current_is_sha) {
            Self::pin(target.sha(), &version, trailing)
        } else {
            Self::named(&version, Named::Tag, current_is_sha, trailing)
        }
    }

    /// Write the tag or branch `new_ref` over a ref followed by `trailing`.
    ///
    /// A label comment keeps its note. A tag written over a tag or branch
    /// gets ` # <new_ref> <note>`. A branch, or a tag written over a SHA pin,
    /// gets ` # <note>` or no comment.
    #[must_use]
    pub fn named(new_ref: &str, kind: Named, current_is_sha: bool, trailing: Trailing<'_>) -> Self {
        match trailing {
            Trailing::Label { note } => Self {
                new_ref: new_ref.to_string(),
                comment: if kind == Named::Tag && !current_is_sha {
                    Some(labeled_comment(new_ref, note))
                } else {
                    note_comment(note)
                },
                to_line_end: true,
            },
            Trailing::None | Trailing::Foreign => Self {
                new_ref: new_ref.to_string(),
                comment: None,
                to_line_end: false,
            },
        }
    }

    /// Pin to `sha` with a ` # <label>` comment.
    ///
    /// A label comment gets `label` and keeps its note, a missing comment is
    /// added, and foreign text after the ref is kept without a comment.
    #[must_use]
    pub fn pin(sha: &str, label: &str, trailing: Trailing<'_>) -> Self {
        let (comment, to_line_end) = match trailing {
            Trailing::None => (Some(labeled_comment(label, "")), false),
            Trailing::Label { note } => (Some(labeled_comment(label, note)), true),
            Trailing::Foreign => (None, false),
        };
        Self {
            new_ref: sha.to_string(),
            comment,
            to_line_end,
        }
    }

    /// Unpin to the tag `marker` names, keeping its note as the comment.
    #[must_use]
    pub fn unpin(marker: PinMarker<'_>) -> Self {
        Self {
            new_ref: marker.tag.to_string(),
            comment: note_comment(marker.note),
            to_line_end: true,
        }
    }

    /// The ref followed by its comment.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}{}", self.new_ref, self.comment.as_deref().unwrap_or(""))
    }

    /// The text written from the start of the ref after which `closer` follows, and whether it runs to the line end.
    ///
    /// A comment after a non-empty `closer`, or a replacement to the line
    /// end, runs to the line end and writes `closer` again without its
    /// trailing whitespace between the ref and the comment.
    #[must_use]
    pub fn splice(&self, closer: &str) -> (String, bool) {
        if (self.comment.is_none() || closer.is_empty()) && !self.to_line_end {
            return (self.text(), false);
        }
        (
            format!(
                "{}{}{}",
                self.new_ref,
                closer.trim_end(),
                self.comment.as_deref().unwrap_or("")
            ),
            true,
        )
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

    const LABEL: Trailing<'static> = Trailing::Label { note: "" };

    const V1: Labels<'static> = Labels::unfetched("v1");

    fn trailing(line: &str) -> Trailing<'_> {
        let ref_end = line.find("@v1").map_or(0, |at| at + 3);
        Trailing::after(line, ref_end, &V1)
    }

    #[test]
    fn trailing_classifies_the_text_after_the_ref() {
        assert_eq!(trailing("  - uses: a/b@v1   # v1.2.0"), LABEL);
        assert_eq!(
            trailing("  - uses: a/b@v1 #v1.2.0   pinned for node16 "),
            Trailing::Label {
                note: "pinned for node16"
            }
        );
        assert_eq!(trailing("  - uses: a/b@v1  "), Trailing::None);
        assert_eq!(trailing("  - { uses: a/b@v1 }"), Trailing::None);
        assert_eq!(trailing("  - uses: a/b@v1 # mine"), Trailing::Foreign);
        assert_eq!(trailing("  - uses: a/b@v1 # 1st step"), Trailing::Foreign);
        assert_eq!(
            trailing("  - uses: a/b@v1 # 1. checkout code"),
            Trailing::Foreign
        );
        assert_eq!(trailing("  - uses: a/b@v1 # lint keep"), Trailing::Foreign);
        assert_eq!(trailing("  - uses: a/b@v1 # TODO fix"), Trailing::Foreign);
        assert_eq!(Trailing::after("  - uses: a/b@v1", 99, &V1), Trailing::None);
    }

    #[test]
    fn labels_name_versions_the_current_ref_and_listed_refs() {
        let mut versions = version::Set::new();
        versions.set_branches(std::collections::HashMap::from([(
            "main".to_string(),
            "c".repeat(40),
        )]));
        versions.set_other_tags(std::collections::HashMap::from([(
            "nightly".to_string(),
            "d".repeat(40),
        )]));
        let unfetched = Labels::unfetched("dev");
        let fetched = Labels::fetched("dev", &versions);
        for (label, by_unfetched, by_fetched) in [
            ("v4.2.0", true, true),
            ("4", true, true),
            ("dev", true, true),
            ("main", false, true),
            ("nightly", false, true),
            ("lint", false, false),
            ("TODO", false, false),
            ("1st", false, false),
        ] {
            assert_eq!(unfetched.names(label), by_unfetched, "{label}");
            assert_eq!(fetched.names(label), by_fetched, "{label}");
        }
        assert!(Labels::checked("dev", true).names("main"));
        assert!(!Labels::checked("dev", false).names("main"));
        assert!(Labels::checked("dev", false).names("dev"));
        let line = "  - uses: a/b@dev # main keep";
        let ref_end = line.find(" #").unwrap_or(0);
        assert_eq!(
            Trailing::after(line, ref_end, &fetched),
            Trailing::Label { note: "keep" }
        );
        assert_eq!(
            Trailing::after(line, ref_end, &unfetched),
            Trailing::Foreign
        );
    }

    #[test]
    fn comment_label_is_the_first_word() {
        assert_eq!(comment_label("# main keep"), Some("main"));
        assert_eq!(comment_label("#\tv4"), Some("v4"));
        assert_eq!(comment_label("#"), None);
        assert_eq!(comment_label("main"), None);
    }

    #[test]
    fn version_comment_starts_with_a_version() {
        for tail in [
            "# v1.2.3",
            "#1.2",
            "# v4",
            "# 1.2.3-beta.1",
            "# v4.0.0 1st step",
            "#\tv4.0.0",
            "# v1.0.0+build",
            "# 1.2.3+build.5 keep",
            "# 1.2.3-beta.1+b",
            "# 1.2.3-0.x-y.10+001.sha-5",
        ] {
            assert!(is_version_comment(tail), "{tail}");
        }
        for tail in [
            "# main",
            "v1.2.3",
            "# 1st step",
            "# 2nd try",
            "# 1. step",
            "# 1.",
            "# 1.foo",
            "# 1.2.3.4",
            "# 1..2",
            "# v",
            "# 1.2-",
            "# 1.x",
            "# 1.2.3+",
            "# 2024-01-01",
            "# v2024-01-01",
            "# 1.2.3-a..b",
            "# 1.2.3-.a",
            "# 1.2.3+.",
            "# 1.2.3+a..b",
            "# 1.2.3-01",
            "# 1.2.3-a.007",
            "# 1.2.3-a_b",
            &format!("# {}", "a".repeat(40)),
            " # v1",
        ] {
            assert!(!is_version_comment(tail), "{tail}");
        }
    }

    #[test]
    fn named_labels_tags_and_keeps_notes() {
        let noted = Trailing::Label { note: "keep" };
        for (kind, is_sha, trailing, expected) in [
            (
                Named::Tag,
                false,
                noted,
                rewrite("v4", Some(" # v4 keep"), true),
            ),
            (Named::Tag, false, LABEL, rewrite("v4", Some(" # v4"), true)),
            (
                Named::Tag,
                true,
                noted,
                rewrite("v4", Some(" # keep"), true),
            ),
            (Named::Tag, true, LABEL, rewrite("v4", None, true)),
            (
                Named::Branch,
                false,
                noted,
                rewrite("v4", Some(" # keep"), true),
            ),
            (Named::Branch, true, LABEL, rewrite("v4", None, true)),
            (
                Named::Tag,
                false,
                Trailing::None,
                rewrite("v4", None, false),
            ),
            (
                Named::Branch,
                true,
                Trailing::Foreign,
                rewrite("v4", None, false),
            ),
        ] {
            assert_eq!(
                Rewrite::named("v4", kind, is_sha, trailing),
                expected,
                "{kind:?} {is_sha} {trailing:?}"
            );
        }
        let sha = "b".repeat(40);
        assert_eq!(
            Rewrite::pin(&sha, "main", noted),
            rewrite(&sha, Some(" # main keep"), true)
        );
        let mut versions = version::Set::new();
        versions.set_other_tags(std::collections::HashMap::from([(
            "nightly".to_string(),
            "d".repeat(40),
        )]));
        versions.set_branches(std::collections::HashMap::from([
            ("main".to_string(), "c".repeat(40)),
            ("nightly".to_string(), "c".repeat(40)),
            ("v2".to_string(), "c".repeat(40)),
        ]));
        assert_eq!(Named::listed("main", &versions), Named::Branch);
        assert_eq!(Named::listed("nightly", &versions), Named::Tag);
        assert_eq!(Named::listed("v9", &versions), Named::Tag);
        assert_eq!(Named::listed("v2", &versions), Named::Branch);
        assert_eq!(Named::listed("develop", &versions), Named::Branch);
        assert_eq!(labeled_comment("v1", ""), " # v1");
        assert_eq!(note_comment(""), None);
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
                LABEL,
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
                LABEL,
                rewrite("v4.2.0", Some(" # v4.2.0"), true),
            ),
            (
                Style::Preserve,
                false,
                Trailing::Label { note: "keep" },
                rewrite("v4.2.0", Some(" # v4.2.0 keep"), true),
            ),
            (
                Style::Preserve,
                true,
                Trailing::Label { note: "keep" },
                rewrite(&sha, Some(" # v4.2.0 keep"), true),
            ),
            (
                Style::Tag,
                true,
                Trailing::Label { note: "keep" },
                rewrite("v4.2.0", Some(" # keep"), true),
            ),
            (
                Style::Tag,
                true,
                Trailing::None,
                rewrite("v4.2.0", None, false),
            ),
            (Style::Tag, true, LABEL, rewrite("v4.2.0", None, true)),
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
    fn rewriting_a_version_comment_keeps_its_note() {
        let target = info("v4.2.0", 'b');
        let sha = "b".repeat(40);
        let noted = Trailing::Label {
            note: "pinned for node16",
        };
        for (style, is_sha, expected) in [
            (
                Style::Preserve,
                true,
                rewrite(&sha, Some(" # v4.2.0 pinned for node16"), true),
            ),
            (
                Style::Sha,
                false,
                rewrite(&sha, Some(" # v4.2.0 pinned for node16"), true),
            ),
            (
                Style::Tag,
                true,
                rewrite("v4.2.0", Some(" # pinned for node16"), true),
            ),
            (
                Style::Tag,
                false,
                rewrite("v4.2.0", Some(" # v4.2.0 pinned for node16"), true),
            ),
            (
                Style::Preserve,
                false,
                rewrite("v4.2.0", Some(" # v4.2.0 pinned for node16"), true),
            ),
        ] {
            assert_eq!(
                Rewrite::outdated(style, is_sha, &target, noted),
                expected,
                "{style} {is_sha}"
            );
        }
        assert_eq!(
            Rewrite::pin(&sha, "v4", noted),
            rewrite(&sha, Some(" # v4 pinned for node16"), true)
        );
        assert_eq!(labeled_comment("v1", ""), " # v1");
        assert_eq!(note_comment(""), None);
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

    #[test]
    fn trailing_skips_the_closer() {
        for (line, closing, trailing) in [
            ("  - uses: \"a/b@v1\" # v1.2.0", "\" ", LABEL),
            ("  - uses: 'a/b@v1' # mine", "' ", Trailing::Foreign),
            ("  - uses: \"a/b@v1\"", "\"", Trailing::None),
            ("  - { uses: 'a/b@v1' }", "' }", Trailing::None),
            ("  - { uses: a/b@v1 } # v1.2.0", " } ", LABEL),
            ("  - [{ uses: a/b@v1 }]  ", " }]  ", Trailing::None),
            (
                "  - { uses: a/b@v1, with: { x: 1 } }",
                ", with: { x: 1 } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { x: 1 } } # v1.0.0 keep",
                ", with: { x: 1 } } ",
                Trailing::Label { note: "keep" },
            ),
            (
                "  - { uses: \"a/b@v1\", with: { x: \"a #b\\\" #c\", y: 'd #e' } } # mine",
                "\", with: { x: \"a #b\\\" #c\", y: 'd #e' } } ",
                Trailing::Foreign,
            ),
            ("  - uses: a/b@v1 # v1.2.0", " ", LABEL),
            (
                "  - uses: 'a/b@v1' # v1.2.0 keep me",
                "' ",
                Trailing::Label { note: "keep me" },
            ),
            ("  - uses: a/b@v1#x", "#x", Trailing::Foreign),
            (
                "  - { uses: a/b@v1, with: { msg: it's } } # v1.0.0",
                ", with: { msg: it's } } ",
                LABEL,
            ),
            (
                "  - { uses: a/b@v1, with: { a: it's, b: 'x # v9' } }",
                ", with: { a: it's, b: 'x # v9' } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { b: 'it''s # v9' } }",
                ", with: { b: 'it''s # v9' } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { b: 'it''s # x' } } # v1.0.0",
                ", with: { b: 'it''s # x' } } ",
                LABEL,
            ),
            (
                "  - { uses: a/b@v1, with: { b: !!str 'x # v9' } }",
                ", with: { b: !!str 'x # v9' } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { b: &a 'x # v9' } }",
                ", with: { b: &a 'x # v9' } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { ? 'x # v9' : y } }",
                ", with: { ? 'x # v9' : y } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { b: !<tag:yaml.org,2002:str> 'x # v9' } }",
                ", with: { b: !<tag:yaml.org,2002:str> 'x # v9' } }",
                Trailing::Foreign,
            ),
            (
                "  - { uses: a/b@v1, with: { b: &a !!str \"x # v9\" } } # v1.0.0",
                ", with: { b: &a !!str \"x # v9\" } } ",
                LABEL,
            ),
            (
                "  - { uses: a/b@v1, with: { b: x!'y } } # v1.0.0",
                ", with: { b: x!'y } } ",
                LABEL,
            ),
        ] {
            let ref_end = line.find("@v1").map_or(0, |at| at + 3);
            assert_eq!(closer(line, ref_end), closing, "{line}");
            assert_eq!(
                after_value(line, ref_end),
                &line[ref_end + closing.len()..],
                "{line}"
            );
            assert_eq!(Trailing::after(line, ref_end, &V1), trailing, "{line}");
        }
    }

    #[test]
    fn splice_writes_the_closer_before_a_comment() {
        for (rewrite, closing, text, to_line_end) in [
            (rewrite("S", None, false), "\" ", "S", false),
            (rewrite("S", Some(" # v2"), false), "\"", "S\" # v2", true),
            (rewrite("S", Some(" # v2"), true), "' } ", "S' } # v2", true),
            (rewrite("v2", None, true), "\" ", "v2\"", true),
            (rewrite("S", Some(" # v2"), false), " ", "S # v2", true),
            (rewrite("S", Some(" # v2"), false), "", "S # v2", false),
            (rewrite("S", None, false), "  ", "S", false),
            (rewrite("S", None, true), "\"\t ", "S\"", true),
            (rewrite("S", Some(" # v2"), true), "", "S # v2", true),
            (
                rewrite("S", Some(" # v2"), true),
                ", with: { x: 1 } } ",
                "S, with: { x: 1 } } # v2",
                true,
            ),
        ] {
            assert_eq!(
                rewrite.splice(closing),
                (text.to_string(), to_line_end),
                "{rewrite:?} {closing:?}"
            );
        }
    }
}
