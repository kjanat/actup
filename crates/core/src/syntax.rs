//! `uses:` extraction for YAML, Markdown and plain-text files.

use std::{collections::BTreeSet, ops::Range, path::Path};

use crate::parser::{
    EditValidationError, UsesLocation, extract_uses_from_yaml, line_spans, lines_ending_in_scalars,
    validate_edit,
};

/// The syntax a file's `uses:` refs are read from, chosen by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// A YAML workflow or action file (`.yml`, `.yaml`).
    Yaml,
    /// A Markdown file (`.md`, `.markdown`); refs come from fenced `yaml` or `yml` blocks.
    Markdown,
    /// Any other file; refs come from `uses:` text on each line.
    Text,
}

impl SourceFormat {
    /// Return the format for `path` by its extension.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml") {
            Self::Yaml
        } else if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") {
            Self::Markdown
        } else {
            Self::Text
        }
    }

    /// Return every `uses:` value in `content`, with lines numbered as [`line_spans`] splits them.
    #[must_use]
    pub fn extract_uses(self, content: &str) -> Vec<UsesLocation> {
        match self {
            Self::Yaml => extract_uses_from_yaml(content),
            Self::Markdown => yaml_blocks(content)
                .into_iter()
                .flat_map(|block| {
                    extract_uses_from_yaml(&block.text)
                        .into_iter()
                        .map(move |loc| block.relocate(&loc))
                })
                .collect(),
            Self::Text => extract_uses_from_text(content),
        }
    }

    /// Return the 1-indexed lines that may take an appended `# comment`.
    ///
    /// YAML lines qualify unless their line break falls inside a scalar.
    /// Markdown lines qualify on the same rule inside fenced YAML blocks.
    /// Plain-text lines never qualify.
    #[must_use]
    pub fn commentable_lines(self, content: &str) -> BTreeSet<usize> {
        match self {
            Self::Yaml => yaml_commentable(content, 1),
            Self::Markdown => yaml_blocks(content)
                .iter()
                .flat_map(|block| yaml_commentable(&block.text, block.first_line))
                .collect(),
            Self::Text => BTreeSet::new(),
        }
    }

    /// Check that `after`, an edit of `before`, keeps its syntax and its number of `uses:` values.
    ///
    /// YAML text must pass [`validate_edit`]. In Markdown, each fenced YAML
    /// block that parsed before the edit must pass [`validate_edit`]. Plain
    /// text is checked for its `uses:` count only.
    ///
    /// # Errors
    ///
    /// Returns the first YAML error, or the two `uses:` counts when they differ.
    pub fn validate_edit(self, before: &str, after: &str) -> Result<(), EditValidationError> {
        match self {
            Self::Yaml => return validate_edit(before, after),
            Self::Markdown => {
                for (old, new) in yaml_blocks(before).iter().zip(&yaml_blocks(after)) {
                    if validate_edit(&old.text, &old.text).is_ok() {
                        validate_edit(&old.text, &new.text)?;
                    }
                }
            }
            Self::Text => {}
        }
        let (before, after) = (
            self.extract_uses(before).len(),
            self.extract_uses(after).len(),
        );
        if before == after {
            Ok(())
        } else {
            Err(EditValidationError::UsesCount { before, after })
        }
    }
}

fn yaml_commentable(yaml: &str, first_line: usize) -> BTreeSet<usize> {
    let in_scalars = lines_ending_in_scalars(yaml);
    (1..=line_spans(yaml).len())
        .filter(|line| !in_scalars.contains(line))
        .map(|line| line + first_line - 1)
        .collect()
}

struct YamlBlock {
    first_line: usize,
    text: String,
}

impl YamlBlock {
    fn relocate(&self, loc: &UsesLocation) -> UsesLocation {
        UsesLocation::new(
            loc.line() + self.first_line - 1,
            loc.column(),
            loc.value().to_owned(),
            loc.raw_line().to_owned(),
        )
    }
}

struct Fence {
    marker: u8,
    len: usize,
}

fn open_fence(line: &str) -> Option<(Fence, &str)> {
    let trimmed = line.trim_start();
    let marker = *trimmed
        .as_bytes()
        .first()
        .filter(|b| matches!(b, b'`' | b'~'))?;
    let len = trimmed.bytes().take_while(|&b| b == marker).count();
    let info = &trimmed[len..];
    (len >= 3 && !(marker == b'`' && info.contains('`'))).then_some((Fence { marker, len }, info))
}

fn closes(fence: &Fence, line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= fence.len && trimmed.bytes().all(|b| b == fence.marker)
}

fn yaml_blocks(content: &str) -> Vec<YamlBlock> {
    let lines: Vec<&str> = line_spans(content)
        .into_iter()
        .map(|span| &content[span])
        .collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while let Some(&line) = lines.get(index) {
        index += 1;
        let line = if index == 1 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let Some((fence, info)) = open_fence(line) else {
            continue;
        };
        let lang = info.split_whitespace().next().unwrap_or("");
        let first_line = index + 1;
        let body_start = index;
        while lines.get(index).is_some_and(|l| !closes(&fence, l)) {
            index += 1;
        }
        let body = lines.get(body_start..index).unwrap_or_default();
        index += 1;
        if lang.eq_ignore_ascii_case("yaml") || lang.eq_ignore_ascii_case("yml") {
            blocks.push(YamlBlock {
                first_line,
                text: body.join("\n"),
            });
        }
    }
    blocks
}

fn extract_uses_from_text(content: &str) -> Vec<UsesLocation> {
    let mut results = Vec::new();
    for (index, span) in line_spans(content).into_iter().enumerate() {
        let line = &content[span];
        for value in uses_values(line) {
            results.push(UsesLocation::new(
                index + 1,
                value.start + 1,
                line[value].to_owned(),
                line.to_owned(),
            ));
        }
    }
    results
}

const QUOTES: [char; 3] = ['"', '\'', '`'];

fn uses_values(line: &str) -> Vec<Range<usize>> {
    let mut values = Vec::new();
    let mut from = 0;
    while let Some(found) = line.get(from..).and_then(|rest| rest.find("uses")) {
        let start = from + found;
        from = start + "uses".len();
        let bounded = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-'));
        let rest = &line[from..];
        let after_key = rest.strip_prefix(QUOTES).unwrap_or(rest);
        let Some(after_colon) = after_key.strip_prefix(':') else {
            continue;
        };
        if !bounded {
            continue;
        }
        let spaced = after_colon.trim_start();
        let quoted = spaced.strip_prefix(QUOTES).unwrap_or(spaced);
        let value_start = line.len() - quoted.len();
        let value_len = quoted
            .find(|c: char| {
                c.is_whitespace() || QUOTES.contains(&c) || matches!(c, ',' | ';' | ')' | '}')
            })
            .unwrap_or(quoted.len());
        let value = value_start..value_start + value_len;
        if line[value.clone()].contains('@') {
            values.push(value);
        }
        from = value_start + value_len;
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(format: SourceFormat, content: &str) -> Vec<(usize, String, Option<String>)> {
        format
            .extract_uses(content)
            .into_iter()
            .map(|loc| {
                let ref_text = loc.ref_range().map(|r| loc.raw_line()[r].to_owned());
                (loc.line(), loc.value().to_owned(), ref_text)
            })
            .collect()
    }

    #[test]
    fn format_follows_the_extension() {
        for (path, format) in [
            ("ci.yml", SourceFormat::Yaml),
            ("a/action.YAML", SourceFormat::Yaml),
            ("README.md", SourceFormat::Markdown),
            ("docs/usage.Markdown", SourceFormat::Markdown),
            ("playground/index.ts", SourceFormat::Text),
            ("infra/github-actions.tf", SourceFormat::Text),
            ("Dockerfile", SourceFormat::Text),
        ] {
            assert_eq!(SourceFormat::from_path(Path::new(path)), format, "{path}");
        }
    }

    #[test]
    fn markdown_reads_only_fenced_yaml_blocks() {
        let md = "\u{feff}```yaml\n- uses: actions/checkout@v6\n```\n\
                  Text uses: prose/not@v1\n\
                  ```sh\nuses: shell/block@v1\n```\n\
                  ~~~~ YML title=ci\r\n  steps:\r\n    - uses: \"actions/cache@v4\"\r\n~~~~~\n\
                  ```yaml\n- uses: open/fence@v2\n";
        assert_eq!(
            found(SourceFormat::Markdown, md),
            [
                (2, "actions/checkout@v6".into(), Some("v6".into())),
                (10, "actions/cache@v4".into(), Some("v4".into())),
                (13, "open/fence@v2".into(), Some("v2".into())),
            ]
        );
    }

    #[test]
    fn markdown_fence_needs_three_markers_and_a_matching_close() {
        let md = "``yaml\n- uses: short/fence@v1\n``\n\
                  ````yaml\n- run: |\n    ```\n  uses: long/fence@v1\n````\n\
                  ```yaml `bad`\n- uses: bad/info@v1\n```\n";
        assert_eq!(
            found(SourceFormat::Markdown, md),
            [(7, "long/fence@v1".into(), Some("v1".into()))]
        );
    }

    #[test]
    fn text_reads_uses_on_each_line() {
        let ts = "const yaml = `\n  - uses: actions/checkout@v6\n`;\n\
                  steps.push({ uses: \"actions/cache@v4\", with: {} });\n\
                  const step = { 'uses': 'a/b@v1' }; // uses: c/d@v2\n\
                  const uses: string = value;\n\
                  reuses: x/y@v1 houses:z/w@v1\n\
                  \"uses\":\"json/style@v3\",\n\
                  uses:\n";
        assert_eq!(
            found(SourceFormat::Text, ts),
            [
                (2, "actions/checkout@v6".into(), Some("v6".into())),
                (4, "actions/cache@v4".into(), Some("v4".into())),
                (5, "a/b@v1".into(), Some("v1".into())),
                (5, "c/d@v2".into(), Some("v2".into())),
                (8, "json/style@v3".into(), Some("v3".into())),
            ]
        );
    }

    #[test]
    fn yaml_delegates_to_the_yaml_walk() {
        let yaml = "steps:\n  - uses: actions/checkout@v4\n  - run: echo uses: x/y@v1\n";
        assert_eq!(
            SourceFormat::Yaml.extract_uses(yaml),
            extract_uses_from_yaml(yaml)
        );
    }

    #[test]
    fn validate_edit_checks_yaml_blocks_and_uses_counts() {
        let yaml = "steps:\n  - uses: a/b@v1\n";
        assert_eq!(
            SourceFormat::Yaml.validate_edit(yaml, "steps:\n  - uses: a/b@v2\n"),
            Ok(())
        );
        assert!(matches!(
            SourceFormat::Yaml.validate_edit(yaml, "steps:\n  - uses: a/b@v2\n  - ["),
            Err(EditValidationError::Syntax(_))
        ));

        let md = "Doc\n```yaml\n- uses: a/b@v1\n```\n```yaml\n{{ broken: [\n- uses: c/d@v1\n```\n";
        let edited = md.replace("a/b@v1", "a/b@v2").replace("c/d@v1", "c/d@v2");
        assert_eq!(SourceFormat::Markdown.validate_edit(md, &edited), Ok(()));
        assert!(matches!(
            SourceFormat::Markdown.validate_edit(md, &md.replace("a/b@v1", "a/b@v2\n  - [")),
            Err(EditValidationError::Syntax(_))
        ));

        let ts = "const a = [{ uses: \"a/b@v1\" }\n";
        assert!(matches!(
            SourceFormat::Yaml.validate_edit(ts, &ts.replace("v1", "v2")),
            Err(EditValidationError::Syntax(_))
        ));
        assert_eq!(
            SourceFormat::Text.validate_edit(ts, &ts.replace("v1", "v2")),
            Ok(())
        );
        assert_eq!(
            SourceFormat::Text.validate_edit(ts, &ts.replace("v1", "v2 uses: c/d@v1")),
            Err(EditValidationError::UsesCount {
                before: 1,
                after: 2
            })
        );
    }

    #[test]
    fn commentable_lines_per_format() {
        let yaml = "a: |\n  x\n  y\nb: c\n";
        assert_eq!(
            SourceFormat::Yaml.commentable_lines(yaml),
            BTreeSet::from([1, 4])
        );
        let md = "# Doc\n```yaml\nk: \"multi\n  line\"\nuses: a/b@v1\n```\n";
        assert_eq!(
            SourceFormat::Markdown.commentable_lines(md),
            BTreeSet::from([4, 5])
        );
        assert_eq!(
            SourceFormat::Text.commentable_lines("uses: a/b@v1\n"),
            BTreeSet::new()
        );
    }
}
