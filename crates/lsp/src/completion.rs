//! Completion provider for `uses:` references.
//!
//! Provides two kinds of completions:
//! - **Action suggestions**: After `uses:` with no or partial text, suggests
//!   popular GitHub Actions from a hardcoded list (plus any actions seen in
//!   the current document's cached diagnostics).
//! - **Version suggestions**: After `owner/repo@`, suggests available versions
//!   fetched from the provider registry (with file cache).
//!
//! Trigger characters: `@` (for version completion after typing the `@`).

use actup_core::{ActionRepoId, provider, version};
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList,
    CompletionResponse, Documentation, MarkupContent, MarkupKind, Position,
};

use crate::file_cache::FileCache;

/// Well-known GitHub Actions suggested after `uses:`.
///
/// These are the most commonly used actions in the ecosystem. The list is
/// intentionally small and hardcoded — it serves as a convenience for common
/// workflows, not a comprehensive registry.
///
/// Last updated: February 2026
const POPULAR_ACTIONS: &[PopularAction] = &[
    PopularAction {
        owner: "actions",
        repo: "checkout",
        description: "Check out a repository",
        default_ref: "v6",
    },
    PopularAction {
        owner: "actions",
        repo: "setup-node",
        description: "Set up Node.js environment",
        default_ref: "v6",
    },
    PopularAction {
        owner: "actions",
        repo: "setup-python",
        description: "Set up Python environment",
        default_ref: "v6",
    },
    PopularAction {
        owner: "actions",
        repo: "cache",
        description: "Cache dependencies and build outputs",
        default_ref: "v5",
    },
    PopularAction {
        owner: "actions",
        repo: "upload-artifact",
        description: "Upload build artifacts",
        default_ref: "v6",
    },
    PopularAction {
        owner: "actions",
        repo: "download-artifact",
        description: "Download build artifacts",
        default_ref: "v7",
    },
    PopularAction {
        owner: "actions",
        repo: "setup-go",
        description: "Set up Go environment",
        default_ref: "v6",
    },
    PopularAction {
        owner: "actions",
        repo: "setup-java",
        description: "Set up Java JDK environment",
        default_ref: "v5",
    },
    PopularAction {
        owner: "actions",
        repo: "github-script",
        description: "Run JavaScript in workflow with GitHub API",
        default_ref: "v8",
    },
    PopularAction {
        owner: "actions",
        repo: "setup-dotnet",
        description: "Set up .NET SDK environment",
        default_ref: "v5",
    },
    PopularAction {
        owner: "docker",
        repo: "build-push-action",
        description: "Build and push Docker images",
        default_ref: "v6",
    },
    PopularAction {
        owner: "docker",
        repo: "login-action",
        description: "Log in to Docker registry",
        default_ref: "v3",
    },
    PopularAction {
        owner: "docker",
        repo: "setup-buildx-action",
        description: "Set up Docker Buildx",
        default_ref: "v3",
    },
];

/// A well-known action for completion suggestions.
struct PopularAction {
    owner: &'static str,
    repo: &'static str,
    description: &'static str,
    default_ref: &'static str,
}

/// Context extracted from the cursor position on a `uses:` line.
#[derive(Debug, PartialEq, Eq)]
enum CompletionContext {
    /// Cursor is after `uses:` with partial or no text (suggest actions).
    /// Contains the text typed so far (may be empty).
    ActionName { prefix: String },
    /// Cursor is after `[host/]owner/repo@` (suggest versions).
    VersionRef {
        /// Repository parsed from the text before `@`, without a subpath.
        repo_id: ActionRepoId,
        /// Text typed after `@` (for filtering).
        version_prefix: String,
    },
}

/// Determine what kind of completion the cursor position calls for.
///
/// Analyzes the line text at the cursor position to decide whether to
/// suggest action names or version refs. The cursor must be within or
/// after the `uses:` value area for completion to activate.
///
/// The incoming `position.character` is in the negotiated LSP encoding;
/// it is converted to a byte offset for internal string operations.
fn detect_completion_context(
    text: &str,
    position: Position,
    encoding: crate::position::PositionEncoding,
) -> Option<CompletionContext> {
    let span = actup_core::line_spans(text)
        .into_iter()
        .nth(position.line as usize)?;
    let line_str = text.get(span)?;

    // Convert incoming cursor column (negotiated encoding) to byte offset.
    let cursor_byte = crate::position::col_to_byte(line_str, position.character, encoding);

    let uses_idx = last_uses_key(line_str.get(..cursor_byte)?)?;
    let after_colon = &line_str[uses_idx + 5..]; // everything after "uses:"

    // Count spaces/tabs after `uses:` (YAML value separator).
    let ws_count = after_colon
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let value_str = &after_colon[ws_count..];
    let value_col = uses_idx + 5 + ws_count;

    // Strip optional quotes.
    let (unquoted, quote) = match value_str.chars().next() {
        Some(q @ ('"' | '\'')) => (&value_str[1..], Some(q)),
        _ => (value_str, None),
    };

    let content_col = value_col + usize::from(quote.is_some());

    // Cursor must be at or after the content start (both in byte offsets now).
    if cursor_byte < content_col {
        return None;
    }

    let value_len = quote
        .map_or_else(
            || unquoted.find(|c: char| c.is_whitespace() || matches!(c, ',' | '}' | ']')),
            |q| unquoted.find(q),
        )
        .unwrap_or(unquoted.len());
    let value_clean = &unquoted[..value_len];

    let offset_in_value = cursor_byte - content_col;
    if offset_in_value > value_len
        && !unquoted
            .get(value_len..offset_in_value)
            .is_some_and(|gap| gap.trim().is_empty())
    {
        return None;
    }
    let typed = value_clean.get(..offset_in_value).unwrap_or(value_clean);

    // Check if there's an `@` in the typed portion.
    if let Some(at_pos) = typed.rfind('@') {
        let before_at = &typed[..at_pos];
        let after_at = &typed[at_pos + 1..];

        if let Ok(repo_id) = actup_core::parse_action_path(before_at) {
            return Some(CompletionContext::VersionRef {
                repo_id: repo_id.without_subpath(),
                version_prefix: after_at.to_owned(),
            });
        }
    }

    // No @ or couldn't parse owner/repo — suggest action names.
    Some(CompletionContext::ActionName {
        prefix: typed.to_owned(),
    })
}

fn last_uses_key(prefix: &str) -> Option<usize> {
    let bytes = prefix.as_bytes();
    let mut quote = None;
    let mut found = None;
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
        let at_boundary = prev.is_none_or(|p| matches!(p, b' ' | b'\t' | b'[' | b'{' | b','));
        match quote {
            Some(b'"') if b == b'\\' => i += 1,
            Some(b'\'') if b == b'\'' && bytes.get(i + 1) == Some(&b'\'') => i += 1,
            Some(q) if b == q => quote = None,
            None if b == b'#' && prev.is_none_or(|p| p == b' ' || p == b'\t') => break,
            None if matches!(b, b'"' | b'\'') && (at_boundary || prev == Some(b':')) => {
                quote = Some(b);
            }
            None if at_boundary
                && bytes
                    .get(i..)
                    .is_some_and(|rest| rest.starts_with(b"uses:")) =>
            {
                found = Some(i);
            }
            Some(_) | None => {}
        }
        i += 1;
    }
    found
}

/// Build completion items for action name suggestions.
///
/// Filters [`POPULAR_ACTIONS`] by the typed prefix (case-insensitive)
/// and produces `CompletionItem`s with the full `owner/repo@default_ref`
/// as the insert text.
fn action_completions(prefix: &str) -> Vec<CompletionItem> {
    let prefix_lower = prefix.to_lowercase();

    POPULAR_ACTIONS
        .iter()
        .filter(|a| {
            if prefix_lower.is_empty() {
                return true;
            }
            let full = format!("{}/{}", a.owner, a.repo);
            full.to_lowercase().contains(&prefix_lower)
        })
        .enumerate()
        .map(|(idx, a)| {
            let label = format!("{}/{}", a.owner, a.repo);

            CompletionItem {
                label: label.clone(),
                kind: Some(CompletionItemKind::MODULE),
                detail: Some(a.description.to_owned()),
                label_details: Some(CompletionItemLabelDetails {
                    detail: Some(format!("@{}", a.default_ref)),
                    description: None,
                }),
                insert_text: Some(format!("{}/{}@{}", a.owner, a.repo, a.default_ref)),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!(
                        "[{}/{}](https://github.com/{}/{})\n\n{}",
                        a.owner, a.repo, a.owner, a.repo, a.description
                    ),
                })),
                // Sort by list order (popular first).
                sort_text: Some(format!("{idx:03}")),
                filter_text: Some(label),
                ..Default::default()
            }
        })
        .collect()
}

const MAX_SEMVER_COMPLETIONS: usize = 20;

/// Build completion items for version suggestions.
///
/// Fetches available versions from the provider registry (with file cache),
/// and produces `CompletionItem`s for the latest version, the major tags, and
/// the newest semver tags matching `version_prefix`.
pub async fn version_completions(
    repo_id: &ActionRepoId,
    version_prefix: &str,
    registry: &provider::Registry,
    file_cache: &FileCache,
    document_versions: Option<&std::collections::HashMap<ActionRepoId, version::Set>>,
) -> Vec<CompletionItem> {
    let versions = match document_versions.and_then(|versions| versions.get(repo_id).cloned()) {
        Some(versions) => versions,
        None => {
            match crate::file_cache::fetch_versions_cached(repo_id, registry, file_cache).await {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    tracing::debug!(repo = ?repo_id, error = %e, "failed to fetch versions for completion");
                    return Vec::new();
                }
                None => return Vec::new(),
            }
        }
    };

    let prefix_lower = version_prefix.to_lowercase();
    let mut items = Vec::new();

    // Add latest version first (preselected).
    if let Some(latest) = versions.latest() {
        let tag = latest.version().as_str();
        if tag.to_lowercase().starts_with(&prefix_lower) || prefix_lower.is_empty() {
            items.push(version_completion_item(
                tag,
                Some("latest"),
                latest.sha(),
                true,
                0,
            ));
        }
    }

    // Add major tags (e.g. v4, v3) — common style for GitHub Actions.
    let mut major_entries: Vec<_> = versions.major_tags().iter().collect();
    major_entries.sort_by(|a, b| b.0.cmp(a.0)); // newest major first
    for (idx, (_major, info)) in major_entries.iter().enumerate() {
        let tag = info.version().as_str();
        if tag.to_lowercase().starts_with(&prefix_lower) || prefix_lower.is_empty() {
            // Skip if already added as latest.
            if items.iter().any(|i| i.label == tag) {
                continue;
            }
            items.push(version_completion_item(
                tag,
                Some("major tag"),
                info.sha(),
                false,
                idx + 1,
            ));
        }
    }

    let semver_tags: Vec<_> = versions
        .semver_tags()
        .iter()
        .filter(|info| {
            let tag = info.version().as_str();
            tag.to_lowercase().starts_with(&prefix_lower) && !items.iter().any(|i| i.label == tag)
        })
        .take(MAX_SEMVER_COMPLETIONS)
        .collect();
    for (idx, info) in semver_tags.into_iter().enumerate() {
        items.push(version_completion_item(
            info.version().as_str(),
            Some("semver"),
            info.sha(),
            false,
            100 + idx,
        ));
    }

    items
}

/// Build a single `CompletionItem` for a version tag.
fn version_completion_item(
    tag: &str,
    detail: Option<&str>,
    sha: &str,
    preselect: bool,
    sort_order: usize,
) -> CompletionItem {
    CompletionItem {
        label: tag.to_owned(),
        kind: Some(CompletionItemKind::VALUE),
        detail: detail.map(str::to_owned),
        label_details: Some(CompletionItemLabelDetails {
            detail: Some(format!(" {}", sha.get(..7).unwrap_or(sha))),
            description: None,
        }),
        preselect: Some(preselect),
        sort_text: Some(format!("{sort_order:04}")),
        ..Default::default()
    }
}

/// Compute completions for the given document and cursor position.
///
/// Entry point called from the `LanguageServer::completion` handler.
/// Returns `None` if the cursor is not in a completable position.
pub async fn compute_completions(
    text: &str,
    position: Position,
    registry: &provider::Registry,
    file_cache: &FileCache,
    document_versions: Option<&std::collections::HashMap<actup_core::ActionRepoId, version::Set>>,
    encoding: crate::position::PositionEncoding,
) -> Option<CompletionResponse> {
    let context = detect_completion_context(text, position, encoding)?;

    match context {
        CompletionContext::ActionName { prefix } => {
            let items = action_completions(&prefix);
            if items.is_empty() {
                None
            } else {
                Some(CompletionResponse::List(CompletionList {
                    // Incomplete: further typing narrows the filter client-side.
                    is_incomplete: false,
                    items,
                }))
            }
        }
        CompletionContext::VersionRef {
            repo_id,
            version_prefix,
        } => {
            let items = version_completions(
                &repo_id,
                &version_prefix,
                registry,
                file_cache,
                document_versions,
            )
            .await;
            if items.is_empty() {
                None
            } else {
                Some(CompletionResponse::List(CompletionList {
                    // Incomplete: network may return more versions on retry.
                    is_incomplete: true,
                    items,
                }))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    const ENC: crate::position::PositionEncoding = crate::position::PositionEncoding::Utf16;

    #[test]
    fn detect_context_empty_after_uses() {
        let text = "      - uses: \n";
        let pos = Position {
            line: 0,
            character: 14,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::ActionName {
                prefix: String::new()
            }
        );
    }

    #[test]
    fn detect_context_partial_action_name() {
        let text = "      - uses: actions/ch\n";
        let pos = Position {
            line: 0,
            character: 24,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::ActionName {
                prefix: "actions/ch".into()
            }
        );
    }

    #[test]
    fn detect_context_version_ref() {
        let text = "      - uses: actions/checkout@v\n";
        let pos = Position {
            line: 0,
            character: 32,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: "v".into(),
            }
        );
    }

    #[test]
    fn detect_context_version_ref_empty_prefix() {
        let text = "      - uses: actions/checkout@\n";
        let pos = Position {
            line: 0,
            character: 31,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: String::new(),
            }
        );
    }

    #[test]
    fn detect_context_quoted_uses() {
        let text = "      - uses: \"actions/checkout@v\"\n";
        let pos = Position {
            line: 0,
            character: 33,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: "v".into(),
            }
        );
    }

    #[test]
    fn detect_context_typed_text_ends_at_cursor() {
        let text = "      - uses: actions/checkout@v4\n";
        for (character, expected) in [
            (
                24,
                CompletionContext::ActionName {
                    prefix: "actions/ch".into(),
                },
            ),
            (
                32,
                CompletionContext::VersionRef {
                    repo_id: ActionRepoId::simple("actions", "checkout"),
                    version_prefix: "v".into(),
                },
            ),
            (
                34,
                CompletionContext::VersionRef {
                    repo_id: ActionRepoId::simple("actions", "checkout"),
                    version_prefix: "v4".into(),
                },
            ),
        ] {
            let pos = Position { line: 0, character };
            assert_eq!(
                detect_completion_context(text, pos, ENC),
                Some(expected),
                "cursor at {character}"
            );
        }
    }

    #[tokio::test]
    async fn compute_completions_mid_version_filters_by_text_before_cursor() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v5.0.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let file_cache = FileCache::disabled();
        let text = "      - uses: actions/checkout@v4\n";
        let position = Position {
            line: 0,
            character: 32,
        };

        let Some(CompletionResponse::List(list)) =
            compute_completions(text, position, &registry, &file_cache, None, ENC).await
        else {
            panic!("expected a completion list");
        };
        let labels: Vec<_> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["v5.0.0"]);
    }

    #[test]
    fn detect_context_returns_none_non_uses_line() {
        let text = "      - run: echo hello\n";
        let pos = Position {
            line: 0,
            character: 15,
        };
        assert!(detect_completion_context(text, pos, ENC).is_none());
    }

    #[test]
    fn detect_context_returns_none_cursor_before_value() {
        let text = "      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 5,
        };
        assert!(detect_completion_context(text, pos, ENC).is_none());
    }

    #[test]
    fn detect_context_parses_the_repo_with_the_uses_host_rule()
    -> Result<(), std::num::TryFromIntError> {
        for (value, repo_id) in [
            (
                "ghe.corp/o/r",
                ActionRepoId::new(Some("ghe.corp".into()), "o", "r", None),
            ),
            (
                "GHE.corp/o/r/sub/dir",
                ActionRepoId::new(Some("ghe.corp".into()), "o", "r", None),
            ),
            (
                "owner/repo/.github/workflows/foo.yml",
                ActionRepoId::simple("owner", "repo"),
            ),
            ("a.b/c", ActionRepoId::simple("a.b", "c")),
        ] {
            let text = format!("      - uses: {value}@v\n");
            let character = u32::try_from(text.len() - 1)?;
            let pos = Position { line: 0, character };
            assert_eq!(
                detect_completion_context(&text, pos, ENC),
                Some(CompletionContext::VersionRef {
                    repo_id,
                    version_prefix: "v".into(),
                }),
                "{value}"
            );
        }
        Ok(())
    }

    #[test]
    fn detect_context_suggests_actions_for_an_invalid_path_before_at()
    -> Result<(), std::num::TryFromIntError> {
        for value in ["justowner", "/repo", "owner/", "./local"] {
            let text = format!("      - uses: {value}@\n");
            let character = u32::try_from(text.len() - 1)?;
            let pos = Position { line: 0, character };
            assert_eq!(
                detect_completion_context(&text, pos, ENC),
                Some(CompletionContext::ActionName {
                    prefix: format!("{value}@"),
                }),
                "{value}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn compute_completions_queries_the_host_named_in_the_ref() {
        use crate::test_utils::{self, DUMMY_SHA};

        let mut registry =
            test_utils::mock_registry(test_utils::MockProvider::new("github.com").with_versions(
                "ghe.corp",
                "o",
                test_utils::simple_version_set("v1.0.0", DUMMY_SHA),
            ));
        registry.register(Box::new(
            test_utils::MockProvider::new("ghe.corp").with_versions(
                "o",
                "r",
                test_utils::simple_version_set("v9.0.0", DUMMY_SHA),
            ),
        ));
        let file_cache = FileCache::disabled();
        let text = "      - uses: ghe.corp/o/r@\n";
        let position = Position {
            line: 0,
            character: 27,
        };

        let Some(CompletionResponse::List(list)) =
            compute_completions(text, position, &registry, &file_cache, None, ENC).await
        else {
            panic!("expected a completion list");
        };
        let labels: Vec<_> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["v9.0.0"]);
    }

    #[test]
    fn action_completions_returns_all_for_empty_prefix() {
        let items = action_completions("");
        assert_eq!(items.len(), POPULAR_ACTIONS.len());
    }

    #[test]
    fn action_completions_filters_by_prefix() {
        let items = action_completions("actions/check");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "actions/checkout");
    }

    #[test]
    fn action_completions_case_insensitive() {
        let items = action_completions("ACTIONS/CHECK");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "actions/checkout");
    }

    #[test]
    fn action_completions_partial_owner() {
        let items = action_completions("docker");
        // Should match all docker/* actions.
        assert!(items.len() >= 3);
        assert!(items.iter().all(|i| i.label.starts_with("docker/")));
    }

    #[test]
    fn action_completion_has_insert_text() {
        let items = action_completions("actions/checkout");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.insert_text.as_deref(), Some("actions/checkout@v6"));
    }

    #[test]
    fn action_completion_has_documentation() {
        let items = action_completions("actions/checkout");
        let item = &items[0];
        assert!(item.documentation.is_some());
        match &item.documentation {
            Some(Documentation::MarkupContent(mc)) => {
                assert!(mc.value.contains("github.com"));
                assert!(mc.value.contains("Check out"));
            }
            _ => panic!("expected MarkupContent documentation"),
        }
    }

    #[test]
    fn action_completion_has_sort_order() {
        let items = action_completions("");
        // First item should sort first.
        assert_eq!(items[0].sort_text.as_deref(), Some("000"));
        assert_eq!(items[1].sort_text.as_deref(), Some("001"));
    }

    #[test]
    fn action_completion_no_match() {
        let items = action_completions("nonexistent/action");
        assert_eq!(items, []);
    }

    #[test]
    fn version_completion_item_preselected() {
        let item = version_completion_item(
            "v4.2.0",
            Some("latest"),
            "abc1234567890abcdef1234567890abcdef123456",
            true,
            0,
        );
        assert_eq!(item.label, "v4.2.0");
        assert_eq!(item.preselect, Some(true));
        assert_eq!(item.detail.as_deref(), Some("latest"));
        assert_eq!(item.kind, Some(CompletionItemKind::VALUE));
    }

    #[test]
    fn version_completion_item_sort_order() {
        let item = version_completion_item("v3.0.0", Some("semver"), "0000000000", false, 5);
        assert_eq!(item.sort_text.as_deref(), Some("0005"));
    }

    #[test]
    fn version_completion_item_sha_label_detail() {
        let item = version_completion_item(
            "v4.0.0",
            None,
            "abc1234567890abcdef1234567890abcdef123456",
            false,
            0,
        );
        let detail = item.label_details.as_ref().unwrap();
        assert_eq!(detail.detail.as_deref(), Some(" abc1234"));
    }

    #[test]
    fn popular_actions_have_valid_data() {
        for action in POPULAR_ACTIONS {
            assert_ne!(action.owner, "");
            assert_ne!(action.repo, "");
            assert_ne!(action.description, "");
            assert!(action.default_ref.starts_with('v'));
        }
    }

    #[tokio::test]
    async fn compute_completions_returns_actions_for_empty_uses() {
        let provider = crate::test_utils::MockProvider::new("github.com");
        let registry = crate::test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();

        let text = "      - uses: \n";
        let position = Position {
            line: 0,
            character: 14,
        };

        let result = compute_completions(text, position, &registry, &file_cache, None, ENC).await;
        let response = result.unwrap();
        match response {
            CompletionResponse::List(list) => {
                assert!(
                    !list.items.is_empty(),
                    "expected action completions for empty uses:"
                );
                // Should include popular actions.
                assert!(list.items.iter().any(|i| i.label.contains("checkout")));
            }
            CompletionResponse::Array(items) => {
                assert_ne!(items, []);
            }
        }
    }

    #[tokio::test]
    async fn compute_completions_returns_none_for_non_uses_line() {
        let provider = crate::test_utils::MockProvider::new("github.com");
        let registry = crate::test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();

        let text = "      - run: echo hello\n";
        let position = Position {
            line: 0,
            character: 15,
        };

        let result = compute_completions(text, position, &registry, &file_cache, None, ENC).await;
        assert!(
            result.is_none(),
            "expected no completions for non-uses line"
        );
    }

    #[tokio::test]
    async fn compute_completions_returns_versions_after_at() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();

        let text = "      - uses: actions/checkout@\n";
        let position = Position {
            line: 0,
            character: 31,
        };

        let result = compute_completions(text, position, &registry, &file_cache, None, ENC).await;
        let response = result.unwrap();
        match response {
            CompletionResponse::List(list) => {
                assert!(
                    !list.items.is_empty(),
                    "expected version completions after @"
                );
                assert!(
                    list.items.iter().any(|i| i.label.contains("v4.2.0")),
                    "expected v4.2.0 in completions, got: {:?}",
                    list.items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );
            }
            CompletionResponse::Array(items) => {
                assert_ne!(items, []);
            }
        }
    }

    #[tokio::test]
    async fn compute_completions_reuses_diagnostics_version_data() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let calls = provider.call_count();
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let config = actup_core::Config::default();
        let uri = tower_lsp_server::ls_types::Uri::from_file_path("/test/ci.yml").unwrap();
        let diagnostics_text = "      - uses: actions/checkout@v3\n";

        let diagnostics = crate::diagnostics::compute_diagnostics(
            diagnostics_text,
            &uri,
            &config,
            &registry,
            &file_cache,
            ENC,
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let completion_text = "      - uses: actions/checkout@\n";
        let position = Position {
            line: 0,
            character: 31,
        };

        let result = compute_completions(
            completion_text,
            position,
            &registry,
            &file_cache,
            Some(&diagnostics.version_sets),
            ENC,
        )
        .await;

        assert!(result.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn version_completions_list_newest_matching_tags_first() {
        use crate::test_utils::{self, DUMMY_SHA};

        let tags: Vec<_> = (0..25)
            .rev()
            .map(|minor| format!("v4.{minor}.0"))
            .chain((0..5).rev().map(|minor| format!("v1.{minor}.0")))
            .map(|tag| test_utils::make_version_info(&tag, DUMMY_SHA))
            .collect();
        let latest = tags.first().cloned();
        let versions = version::Set::build(
            tags,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            latest,
        );
        let repo_id = ActionRepoId::simple("a", "b");
        let document_versions = std::collections::HashMap::from([(repo_id.clone(), versions)]);
        let registry = test_utils::mock_registry(test_utils::MockProvider::new("github.com"));
        let file_cache = FileCache::disabled();
        let labels = async |prefix: &str| -> Vec<String> {
            version_completions(
                &repo_id,
                prefix,
                &registry,
                &file_cache,
                Some(&document_versions),
            )
            .await
            .into_iter()
            .map(|item| item.label)
            .collect()
        };

        let newest_v4: Vec<_> = (3..25).rev().map(|minor| format!("v4.{minor}.0")).collect();
        assert_eq!(labels("").await, newest_v4[..=MAX_SEMVER_COMPLETIONS]);
        assert_eq!(labels("v4").await, newest_v4[..=MAX_SEMVER_COMPLETIONS]);
        assert_eq!(
            labels("v1").await,
            ["v1.4.0", "v1.3.0", "v1.2.0", "v1.1.0", "v1.0.0"]
        );

        let details: Vec<_> = version_completions(
            &repo_id,
            "v4",
            &registry,
            &file_cache,
            Some(&document_versions),
        )
        .await
        .into_iter()
        .map(|item| item.detail)
        .collect();
        let Some((first, rest)) = details.split_first() else {
            panic!("no completions for v4");
        };
        assert_eq!(first.as_deref(), Some("latest"));
        assert!(
            rest.iter()
                .all(|detail| detail.as_deref() == Some("semver"))
        );
    }

    #[test]
    fn detect_context_multiline() {
        let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@\n";
        let pos = Position {
            line: 6,
            character: 31,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: String::new(),
            }
        );
    }

    #[test]
    fn detect_context_counts_a_lone_cr_as_a_line_break() {
        let text = "x: 1\r      - uses: actions/checkout@\r\n";
        let pos = Position {
            line: 1,
            character: 31,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: String::new(),
            })
        );
    }

    #[test]
    fn detect_context_single_quoted_uses() {
        let text = "      - uses: 'actions/checkout@v'\n";
        let pos = Position {
            line: 0,
            character: 33,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: "v".into(),
            }
        );
    }

    #[test]
    fn detect_context_cursor_at_colon() {
        let text = "      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 13, // right at the colon
        };
        assert!(detect_completion_context(text, pos, ENC).is_none());
    }

    #[test]
    fn action_completion_has_module_kind() {
        let items = action_completions("actions/checkout");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, Some(CompletionItemKind::MODULE));
    }

    #[test]
    fn version_completion_item_has_value_kind() {
        let item = version_completion_item("v1.0.0", Some("semver"), "abc1234", false, 0);
        assert_eq!(item.kind, Some(CompletionItemKind::VALUE));
    }

    #[test]
    fn version_completion_item_truncates_long_sha() {
        let long_sha = "abc1234567890abcdef1234567890abcdef123456";
        let item = version_completion_item("v1.0.0", None, long_sha, false, 0);
        let detail = item.label_details.as_ref().unwrap();
        // Should show first 7 chars of SHA.
        assert!(detail.detail.as_ref().unwrap().contains("abc1234"));
        assert!(!detail.detail.as_ref().unwrap().contains("567890"));
    }

    #[test]
    fn action_completions_returns_sorted_by_index() {
        let items = action_completions("");
        assert_eq!(items[0].sort_text.as_deref(), Some("000"));
        assert_eq!(items[1].sort_text.as_deref(), Some("001"));
        assert_eq!(items[2].sort_text.as_deref(), Some("002"));
    }

    #[test]
    fn detect_context_flow_mapping_stops_at_comma() {
        let text = "      - { uses: actions/checkout@v, with: { x: 1 } }\n";
        let pos = Position {
            line: 0,
            character: 34,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: "v".into(),
            })
        );
    }

    #[test]
    fn detect_context_flow_mapping_stops_at_brace() {
        let text = "      - {uses: actions/checkout@}\n";
        let pos = Position {
            line: 0,
            character: 32,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: String::new(),
            })
        );
    }

    #[test]
    fn detect_context_quoted_flow_value_stops_at_closing_quote() {
        let text = "      - { uses: \"actions/checkout@v\", with: { x: 1 } }\n";
        let pos = Position {
            line: 0,
            character: 35,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "checkout"),
                version_prefix: "v".into(),
            })
        );
    }

    #[test]
    fn detect_context_flow_sequence_uses_value_under_cursor() {
        let text = "steps: [{ uses: a/b@v4 }, { uses: actions/setup-node@v }]\n";
        let pos = Position {
            line: 0,
            character: 54,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "setup-node"),
                version_prefix: "v".into(),
            })
        );
    }

    #[test]
    fn detect_context_none_past_flow_value() {
        let text = "      - { uses: actions/checkout@v4, with: { x: 1 } }\n";
        for character in [36, 41, 52] {
            let pos = Position { line: 0, character };
            assert_eq!(
                detect_completion_context(text, pos, ENC),
                None,
                "cursor at {character}"
            );
        }
    }

    #[test]
    fn detect_context_none_past_closing_quote_or_comment() {
        for (text, character) in [
            ("      - uses: \"actions/checkout@v4\" # c\n", 37),
            ("      - uses: actions/checkout@v4 # pinned\n", 40),
        ] {
            let pos = Position { line: 0, character };
            assert_eq!(detect_completion_context(text, pos, ENC), None, "{text}");
        }
    }

    #[test]
    fn detect_context_ignores_uses_text_outside_a_key() {
        for (text, character) in [
            ("      - { uses: a/b@v1, name: \"uses: x\" }\n", 39),
            ("      - { uses: a/b@v1, name: 'uses: x' }\n", 39),
            ("      - { uses: a/b@v1, reuses: x }\n", 33),
            ("      - { uses: a/b@v1 } # uses: x\n", 35),
        ] {
            let pos = Position { line: 0, character };
            assert_eq!(detect_completion_context(text, pos, ENC), None, "{text}");
        }
    }

    #[test]
    fn detect_context_ignores_uses_in_escaped_quotes_and_dashed_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        for text in [
            "      - { uses: a/b@v1, name: 'it''s uses: x' }\n",
            "      - { uses: a/b@v1, pre-uses: x }\n",
        ] {
            let x = text.rfind('x').ok_or("no x")?;
            let pos = Position {
                line: 0,
                character: u32::try_from(x + 1)?,
            };
            assert_eq!(detect_completion_context(text, pos, ENC), None, "{text}");
        }
        let text = "      - { name: 'it''s', uses: a/b@v }\n";
        let pos = Position {
            line: 0,
            character: u32::try_from(text.rfind('v').ok_or("no v")? + 1)?,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("a", "b"),
                version_prefix: "v".into(),
            })
        );
        Ok(())
    }

    #[test]
    fn detect_context_value_ended_by_bracket() {
        let text = "steps: [uses: actions/setup-node@v]\n";
        let pos = Position {
            line: 0,
            character: 34,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("actions", "setup-node"),
                version_prefix: "v".into(),
            })
        );
        let past = Position {
            line: 0,
            character: 35,
        };
        assert_eq!(detect_completion_context(text, past, ENC), None);
    }

    #[test]
    fn detect_context_apostrophe_inside_a_word_opens_no_quote() {
        let text = "      - { name: don't, uses: a/b@v }\n";
        let pos = Position {
            line: 0,
            character: 34,
        };
        assert_eq!(
            detect_completion_context(text, pos, ENC),
            Some(CompletionContext::VersionRef {
                repo_id: ActionRepoId::simple("a", "b"),
                version_prefix: "v".into(),
            })
        );
    }

    #[test]
    fn detect_context_trailing_spaces_after_at() {
        let text = "      - uses: actions/checkout@   \n";
        let pos = Position {
            line: 0,
            character: 32,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert!(matches!(ctx, CompletionContext::VersionRef { .. }));
    }
}
