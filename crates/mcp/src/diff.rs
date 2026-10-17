//! Unified diffs for edits that keep every line in place.

use std::{fmt::Write, ops::Range};

use actup_core::line_spans;

const CONTEXT: usize = 3;

/// Render a unified diff of `old` to `new` for `path`.
///
/// Both texts must split into the same number of lines, which holds for
/// in-line ref edits. Returns `None` when they differ in line count, and an
/// empty string when they are equal.
#[must_use]
pub fn render(path: &str, old: &str, new: &str) -> Option<String> {
    let old_lines = lines(old);
    let new_lines = lines(new);
    if old_lines.len() != new_lines.len() {
        return None;
    }
    let changed: Vec<usize> = (0..old_lines.len())
        .filter(|&i| old_lines[i] != new_lines[i])
        .collect();
    let mut out = String::new();
    if changed.is_empty() {
        return Some(out);
    }
    let last = old_lines.len() - 1;
    let old_open = !ends_with_break(old);
    let new_open = !ends_with_break(new);
    let _ = write!(out, "--- a/{path}\n+++ b/{path}\n");
    for hunk in hunks(&changed, old_lines.len()) {
        let start = hunk.start + 1;
        let len = hunk.len();
        let _ = writeln!(out, "@@ -{start},{len} +{start},{len} @@");
        let mut i = hunk.start;
        while i < hunk.end {
            if old_lines[i] == new_lines[i] {
                line(&mut out, ' ', old_lines[i], i == last && old_open);
                i += 1;
                continue;
            }
            let run_end = (i..hunk.end)
                .find(|&j| old_lines[j] == new_lines[j])
                .unwrap_or(hunk.end);
            for (j, text) in old_lines[i..run_end].iter().enumerate() {
                line(&mut out, '-', text, i + j == last && old_open);
            }
            for (j, text) in new_lines[i..run_end].iter().enumerate() {
                line(&mut out, '+', text, i + j == last && new_open);
            }
            i = run_end;
        }
    }
    Some(out)
}

fn lines(text: &str) -> Vec<&str> {
    line_spans(text)
        .into_iter()
        .map(|span| &text[span])
        .collect()
}

fn ends_with_break(text: &str) -> bool {
    text.is_empty() || text.ends_with('\n') || text.ends_with('\r')
}

fn line(out: &mut String, mark: char, text: &str, open: bool) {
    out.push(mark);
    out.push_str(text);
    out.push('\n');
    if open {
        out.push_str("\\ No newline at end of file\n");
    }
}

fn hunks(changed: &[usize], count: usize) -> Vec<Range<usize>> {
    let mut hunks: Vec<Range<usize>> = Vec::new();
    for &i in changed {
        let start = i.saturating_sub(CONTEXT);
        let end = (i + CONTEXT + 1).min(count);
        match hunks.last_mut() {
            Some(last) if start <= last.end => last.end = end,
            _ => hunks.push(start..end),
        }
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_empty_for_equal_text() {
        assert_eq!(render("a.yml", "x\n", "x\n").as_deref(), Some(""));
    }

    #[test]
    fn render_refuses_a_line_count_change() {
        assert_eq!(render("a.yml", "x\n", "x\ny\n"), None);
    }

    #[test]
    fn render_shows_three_lines_of_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = old.replace('5', "five");
        assert_eq!(
            render("ci.yml", old, &new).as_deref(),
            Some(
                "--- a/ci.yml\n+++ b/ci.yml\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n"
            )
        );
    }

    #[test]
    fn render_merges_close_changes_and_splits_distant_ones() {
        let old = (1..=20)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let new = old
            .replace("\n2\n", "\nB\n")
            .replace("\n3\n", "\nC\n")
            .replace("\n9\n", "\nI\n")
            .replace("\n19\n", "\nS\n");
        let Some(diff) = render("f", &old, &new) else {
            panic!("line counts match");
        };
        assert_eq!(
            diff,
            "--- a/f\n+++ b/f\n\
             @@ -1,12 +1,12 @@\n 1\n-2\n-3\n+B\n+C\n 4\n 5\n 6\n 7\n 8\n-9\n+I\n 10\n 11\n 12\n\
             @@ -16,5 +16,5 @@\n 16\n 17\n 18\n-19\n+S\n 20\n"
        );
    }

    #[test]
    fn render_marks_a_missing_final_newline() {
        assert_eq!(
            render("f", "a\nb", "a\nB").as_deref(),
            Some(
                "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+B\n\\ No newline at end of file\n"
            )
        );
        assert_eq!(
            render("f", "A\nb", "a\nb").as_deref(),
            Some("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-A\n+a\n b\n\\ No newline at end of file\n")
        );
    }

    #[test]
    fn render_splits_crlf_lines() {
        assert_eq!(
            render("f", "a\r\nb\r\n", "a\r\nB\r\n").as_deref(),
            Some("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n")
        );
    }
}
