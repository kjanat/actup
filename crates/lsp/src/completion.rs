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

use actup_core::{provider, version};
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
    /// Cursor is after `owner/repo@` (suggest versions).
    /// Contains the owner and repo parsed from the line.
    VersionRef {
        owner: String,
        repo: String,
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
    let line_str = text.lines().nth(position.line as usize)?;

    // Convert incoming cursor column (negotiated encoding) to byte offset.
    let cursor_byte = crate::position::col_to_byte(line_str, position.character, encoding);

    // Find `uses:` on this line.
    let uses_idx = line_str.find("uses:")?;
    let after_colon = &line_str[uses_idx + 5..]; // everything after "uses:"

    // Count spaces/tabs after `uses:` (YAML value separator).
    let ws_count = after_colon
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let value_str = &after_colon[ws_count..];
    let value_col = uses_idx + 5 + ws_count;

    // Strip optional quotes.
    let (unquoted, quote_offset) = if value_str.starts_with('"') || value_str.starts_with('\'') {
        (&value_str[1..], 1usize)
    } else {
        (value_str, 0usize)
    };

    let content_col = value_col + quote_offset;

    // Cursor must be at or after the content start (both in byte offsets now).
    if cursor_byte < content_col {
        return None;
    }

    // Clean trailing quote/whitespace from the full value for analysis.
    let value_clean = unquoted
        .trim_end()
        .trim_end_matches('"')
        .trim_end_matches('\'');

    // Extract the portion from content start up to and including the cursor.
    let offset_in_value = cursor_byte - content_col;
    let typed = if offset_in_value < value_clean.len() {
        // Ensure we don't slice inside a multi-byte character (defensive).
        let end = offset_in_value + 1;
        value_clean.get(..end).unwrap_or(value_clean)
    } else {
        value_clean
    };

    // Check if there's an `@` in the typed portion.
    if let Some(at_pos) = typed.rfind('@') {
        let before_at = &typed[..at_pos];
        let after_at = &typed[at_pos + 1..];

        // Parse owner/repo from the part before @.
        if let Some((owner, repo)) = parse_owner_repo(before_at) {
            return Some(CompletionContext::VersionRef {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                version_prefix: after_at.to_owned(),
            });
        }
    }

    // No @ or couldn't parse owner/repo — suggest action names.
    Some(CompletionContext::ActionName {
        prefix: typed.to_owned(),
    })
}

/// Parse `owner/repo` from a string, handling optional subpath.
///
/// Accepts: `owner/repo`, `owner/repo/subpath/...`
/// Returns `(owner, repo)` or `None` if the format is invalid.
fn parse_owner_repo(s: &str) -> Option<(&str, &str)> {
    let mut parts = s.splitn(3, '/');
    let owner = parts.next().filter(|o| !o.is_empty())?;
    let repo = parts.next().filter(|r| !r.is_empty())?;
    Some((owner, repo))
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

/// Build completion items for version suggestions.
///
/// Fetches available versions from the provider registry (with file cache),
/// and produces `CompletionItem`s for each known semver tag plus major tags.
pub async fn version_completions(
    owner: &str,
    repo: &str,
    version_prefix: &str,
    registry: &provider::Registry,
    file_cache: &FileCache,
    document_versions: Option<&std::collections::HashMap<actup_core::ActionRepoId, version::Set>>,
) -> Vec<CompletionItem> {
    let repo_id = actup_core::ActionRepoId::new(None, owner.to_owned(), repo.to_owned(), None);

    let versions = match document_versions.and_then(|versions| versions.get(&repo_id).cloned()) {
        Some(versions) => versions,
        None => {
            match crate::file_cache::fetch_versions_cached(&repo_id, registry, file_cache).await {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    tracing::debug!(owner, repo, error = %e, "failed to fetch versions for completion");
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

    // Add semver tags (newest first, limited to avoid overwhelming).
    let semver_tags = versions.semver_tags();
    let max_semver = 20;
    for (idx, info) in semver_tags.iter().rev().take(max_semver).enumerate() {
        let tag = info.version().as_str();
        if tag.to_lowercase().starts_with(&prefix_lower) || prefix_lower.is_empty() {
            if items.iter().any(|i| i.label == tag) {
                continue;
            }
            let detail = if versions.latest().is_some_and(|l| l == info) {
                "latest"
            } else {
                "semver"
            };
            items.push(version_completion_item(
                tag,
                Some(detail),
                info.sha(),
                false,
                100 + idx,
            ));
        }
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
            owner,
            repo,
            version_prefix,
        } => {
            let items = version_completions(
                &owner,
                &repo,
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
            character: 23,
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
            character: 31,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                owner: "actions".into(),
                repo: "checkout".into(),
                version_prefix: "v".into(),
            }
        );
    }

    #[test]
    fn detect_context_version_ref_empty_prefix() {
        let text = "      - uses: actions/checkout@\n";
        let pos = Position {
            line: 0,
            character: 30,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                owner: "actions".into(),
                repo: "checkout".into(),
                version_prefix: String::new(),
            }
        );
    }

    #[test]
    fn detect_context_quoted_uses() {
        let text = "      - uses: \"actions/checkout@v\"\n";
        let pos = Position {
            line: 0,
            character: 32,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                owner: "actions".into(),
                repo: "checkout".into(),
                version_prefix: "v".into(),
            }
        );
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
    fn parse_owner_repo_simple() {
        let (owner, repo) = parse_owner_repo("actions/checkout").unwrap();
        assert_eq!(owner, "actions");
        assert_eq!(repo, "checkout");
    }

    #[test]
    fn parse_owner_repo_with_subpath() {
        let (owner, repo) = parse_owner_repo("owner/repo/.github/workflows/foo.yml").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_owner_repo_invalid() {
        assert!(parse_owner_repo("justowner").is_none());
        assert!(parse_owner_repo("").is_none());
        assert!(parse_owner_repo("/repo").is_none());
        assert!(parse_owner_repo("owner/").is_none());
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
        assert!(items.is_empty());
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
            assert!(!action.owner.is_empty());
            assert!(!action.repo.is_empty());
            assert!(!action.description.is_empty());
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
                assert!(!items.is_empty());
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
            character: 30,
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
                assert!(!items.is_empty());
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
            character: 30,
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

    #[test]
    fn detect_context_multiline() {
        let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@\n";
        let pos = Position {
            line: 6,
            character: 30,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                owner: "actions".into(),
                repo: "checkout".into(),
                version_prefix: String::new(),
            }
        );
    }

    #[test]
    fn detect_context_single_quoted_uses() {
        let text = "      - uses: 'actions/checkout@v'\n";
        let pos = Position {
            line: 0,
            character: 32,
        };
        let ctx = detect_completion_context(text, pos, ENC).unwrap();
        assert_eq!(
            ctx,
            CompletionContext::VersionRef {
                owner: "actions".into(),
                repo: "checkout".into(),
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
