//! Hover information for `uses:` references.
//!
//! Shows action metadata (name, description, author), version status,
//! repository link, and deprecation warnings on hover over `uses:` lines.

use std::fmt::Write as _;
use std::sync::Arc;

use actup_core::resolution::CheckStatus;
use actup_core::{ActionRef, Config, check_action, extract_uses_from_yaml, parse_uses, provider};
use dashmap::DashMap;
use tower_lsp_server::ls_types::{
    Hover, HoverContents, MarkupContent, MarkupKind, Position, Range,
};

use crate::file_cache::FileCache;

/// Cached action.yml metadata for hover display.
///
/// Stored in an in-memory cache keyed by `ActionRepoId` + ref to avoid
/// re-fetching on every hover request. The provider's `get_action_yml`
/// is not cached by `CachingProvider`, so we cache results ourselves.
#[derive(Debug, Clone)]
pub struct CachedActionMeta {
    /// Action name from action.yml (e.g. "Checkout").
    pub name: Option<String>,
    /// Action description from action.yml.
    pub description: Option<String>,
    /// Action author from action.yml.
    pub author: Option<String>,
    /// Deprecated inputs: (`input_name`, `deprecation_message`).
    pub deprecated_inputs: Vec<(String, String)>,
    /// Runtime (e.g. "node20", "composite", "docker").
    pub runtime: Option<String>,
}

/// Key for the action metadata cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionMetaCacheKey {
    owner: String,
    repo: String,
    ref_str: String,
    host: Option<String>,
}

impl ActionMetaCacheKey {
    fn from_action_ref(action_ref: &ActionRef) -> Self {
        Self {
            owner: action_ref.owner().to_owned(),
            repo: action_ref.repo().to_owned(),
            ref_str: action_ref.ref_str().to_owned(),
            host: action_ref.host().map(str::to_owned),
        }
    }
}

/// In-memory cache for action.yml metadata used by hover.
///
/// This cache is unbounded, but in practice grows slowly: one entry per
/// unique (owner, repo, ref) triple the user hovers over in a session.
/// A typical session might see 10–50 distinct actions, so memory impact
/// is negligible. If this ever becomes a concern, an LRU wrapper could
/// be added with minimal API change.
pub type ActionMetaCache = DashMap<ActionMetaCacheKey, Arc<CachedActionMeta>>;

/// Create a new empty action metadata cache.
pub fn new_action_meta_cache() -> ActionMetaCache {
    DashMap::new()
}

/// A parsed action located at a specific position in the document.
struct LocatedActionForHover {
    action_ref: ActionRef,
    /// 0-indexed line number.
    line: u32,
    /// Byte range of the full `uses:` value in the line.
    value_col_start: u32,
    value_col_end: u32,
    /// The original `uses:` value string.
    #[allow(dead_code)]
    uses_value: String,
}

/// Find the action reference at the given cursor position in the document.
///
/// Returns `None` if the cursor is not on a `uses:` value.
fn find_action_at_position(text: &str, position: Position) -> Option<LocatedActionForHover> {
    let locations = extract_uses_from_yaml(text);

    for loc in locations {
        // Convert 1-indexed line to 0-indexed.
        #[allow(clippy::cast_possible_truncation)]
        let line_0 = (loc.line() - 1) as u32;

        if line_0 != position.line {
            continue;
        }

        // Check if cursor is within the value range.
        // LSP positions use UTF-16 code units, but `uses:` values are
        // ASCII-only (`owner/repo@ref`), so byte offsets == UTF-16 offsets.
        let value_range = loc.value_range();
        #[allow(clippy::cast_possible_truncation)]
        let col_start = value_range.start as u32;
        #[allow(clippy::cast_possible_truncation)]
        let col_end = value_range.end as u32;

        if position.character < col_start || position.character >= col_end {
            continue;
        }

        // Parse the uses value — skip local/docker refs.
        if let Ok(Some(action_ref)) = parse_uses(loc.value()) {
            return Some(LocatedActionForHover {
                action_ref,
                line: line_0,
                value_col_start: col_start,
                value_col_end: col_end,
                uses_value: loc.value().to_string(),
            });
        }
    }

    None
}

/// Fetch or retrieve cached action.yml metadata.
async fn get_action_meta(
    action_ref: &ActionRef,
    registry: &provider::Registry,
    cache: &ActionMetaCache,
) -> Option<Arc<CachedActionMeta>> {
    let cache_key = ActionMetaCacheKey::from_action_ref(action_ref);

    // Check cache first.
    if let Some(cached) = cache.get(&cache_key) {
        return Some(Arc::clone(cached.value()));
    }

    // Fetch from provider.
    let repo_id = action_ref.to_repo_id();
    let provider = registry.find_for_repo(&repo_id)?;

    let action_yml = match provider
        .get_action_yml(&repo_id, action_ref.ref_str())
        .await
    {
        Ok(yml) => yml,
        Err(e) => {
            tracing::debug!(
                action = %action_ref,
                error = %e,
                "failed to fetch action.yml for hover"
            );
            return None;
        }
    };

    let deprecated_inputs: Vec<(String, String)> = action_yml
        .inputs()
        .iter()
        .filter_map(|(name, input)| {
            input
                .deprecation_message()
                .map(|msg| (name.clone(), msg.to_owned()))
        })
        .collect();

    let runtime = action_yml.runs().map(|r| r.using().to_owned());

    let meta = Arc::new(CachedActionMeta {
        name: action_yml.name().map(str::to_owned),
        description: action_yml.description().map(str::to_owned),
        author: action_yml.author().map(str::to_owned),
        deprecated_inputs,
        runtime,
    });

    cache.insert(cache_key, Arc::clone(&meta));
    Some(meta)
}

/// Build the repository URL for an action.
fn repo_url(action_ref: &ActionRef, default_host: Option<&str>) -> String {
    let host = action_ref.host().or(default_host).unwrap_or("github.com");
    format!(
        "https://{}/{}/{}",
        host,
        action_ref.owner(),
        action_ref.repo()
    )
}

/// Format version status information for hover display.
fn format_version_status(status: &CheckStatus) -> String {
    match status {
        CheckStatus::UpToDate => "Up to date".to_owned(),
        CheckStatus::Outdated { current, target } => {
            format!(
                "**Outdated**: {} -> {} (`{}`)",
                current,
                target.version(),
                target.sha().get(..7).unwrap_or_else(|| target.sha()),
            )
        }
        CheckStatus::Pinned { sha, tag } => {
            let tag_info = tag
                .as_ref()
                .map_or_else(|| "unknown version".into(), String::clone);
            format!(
                "**Pinned** to `{}` ({tag_info})",
                sha.get(..12).unwrap_or(sha),
            )
        }
        CheckStatus::Ignored => "Ignored by config".to_owned(),
        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => {
            if *is_environment_error {
                format!("Check failed: {reason}")
            } else {
                format!("**Unresolvable**: {reason}")
            }
        }
    }
}

/// Build the full hover markdown content.
fn build_hover_content(
    action_ref: &ActionRef,
    meta: Option<&CachedActionMeta>,
    status: &CheckStatus,
    latest_version: Option<&str>,
    default_host: Option<&str>,
) -> String {
    let mut md = String::with_capacity(512);

    // Header: action name or owner/repo
    if let Some(meta) = meta
        && let Some(name) = &meta.name
    {
        let _ = writeln!(md, "### {name}");
    } else {
        let _ = writeln!(md, "### {}/{}", action_ref.owner(), action_ref.repo());
    }

    // Description
    if let Some(meta) = meta
        && let Some(desc) = &meta.description
    {
        let _ = writeln!(md, "\n{desc}");
    }

    // Author
    if let Some(meta) = meta
        && let Some(author) = &meta.author
    {
        let _ = writeln!(md, "\n*by {author}*");
    }

    let _ = writeln!(md);

    // Version info section
    let _ = writeln!(md, "---");
    let _ = writeln!(md, "| | |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| **Current** | `{}` |", action_ref.ref_str());

    if let Some(latest) = latest_version {
        let _ = writeln!(md, "| **Latest** | `{latest}` |");
    }

    let _ = writeln!(md, "| **Status** | {} |", format_version_status(status));

    // Runtime
    if let Some(meta) = meta
        && let Some(runtime) = &meta.runtime
    {
        let _ = writeln!(md, "| **Runtime** | {runtime} |");
    }

    // Repository link
    let url = repo_url(action_ref, default_host);
    let _ = writeln!(md, "\n[View on GitHub]({url})");

    // Deprecation warnings
    if let Some(meta) = meta
        && !meta.deprecated_inputs.is_empty()
    {
        let _ = writeln!(md, "\n---");
        let _ = writeln!(md, "**Deprecated inputs:**");
        for (input_name, message) in &meta.deprecated_inputs {
            let _ = writeln!(md, "- `{input_name}`: {message}");
        }
    }

    md
}

/// Compute hover information for a position in a document.
///
/// Returns `None` if the cursor is not on a `uses:` reference or if the
/// reference is local/docker.
pub async fn compute_hover(
    text: &str,
    position: Position,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    meta_cache: &ActionMetaCache,
) -> Option<Hover> {
    let located = find_action_at_position(text, position)?;

    // Fetch version info via shared cache-then-fetch helper.
    let repo_id = located.action_ref.to_repo_id();
    let versions =
        match crate::file_cache::fetch_versions_cached(&repo_id, registry, file_cache).await {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                tracing::debug!(error = %e, "failed to fetch versions for hover");
                None
            }
            None => None,
        };

    // Check status
    let status = versions.as_ref().map_or_else(
        || CheckStatus::Unresolvable {
            reason: "no provider available".into(),
            is_environment_error: true,
        },
        |v| {
            check_action(&located.action_ref, v, config)
                .status()
                .clone()
        },
    );

    let latest_version = versions
        .as_ref()
        .and_then(|v| v.latest())
        .map(|info| info.version().to_string());

    // Fetch action.yml metadata (cached).
    let meta = get_action_meta(&located.action_ref, registry, meta_cache).await;

    let default_host = registry.default_host();
    let content = build_hover_content(
        &located.action_ref,
        meta.as_deref(),
        &status,
        latest_version.as_deref(),
        default_host,
    );

    let range = Range {
        start: Position {
            line: located.line,
            character: located.value_col_start,
        },
        end: Position {
            line: located.line,
            character: located.value_col_end,
        },
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: content,
        }),
        range: Some(range),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actup_core::resolution::CheckStatus;
    use actup_core::version;

    #[test]
    fn find_action_at_cursor_on_uses_line() {
        let yaml = "      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 20,
        };
        let located = find_action_at_position(yaml, pos).unwrap();
        assert_eq!(located.action_ref.owner(), "actions");
        assert_eq!(located.action_ref.repo(), "checkout");
        assert_eq!(located.action_ref.ref_str(), "v4");
    }

    #[test]
    fn find_action_returns_none_off_uses_value() {
        let yaml = "      - uses: actions/checkout@v4\n";
        // Position before the value.
        let pos = Position {
            line: 0,
            character: 5,
        };
        assert!(find_action_at_position(yaml, pos).is_none());
    }

    #[test]
    fn find_action_returns_none_wrong_line() {
        let yaml = "name: CI\n      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 20,
        };
        assert!(find_action_at_position(yaml, pos).is_none());
    }

    #[test]
    fn find_action_returns_none_for_local_ref() {
        let yaml = "      - uses: ./local-action\n";
        let pos = Position {
            line: 0,
            character: 18,
        };
        assert!(find_action_at_position(yaml, pos).is_none());
    }

    #[test]
    fn find_action_at_end_of_value() {
        let yaml = "      - uses: actions/checkout@v4\n";
        // Character at the end boundary of the value.
        let pos = Position {
            line: 0,
            character: 33, // right at end
        };
        // Should still be within bounds (inclusive end).
        // value is "actions/checkout@v4" = 19 chars, starts at col 14, ends at 33
        let located = find_action_at_position(yaml, pos);
        // 33 == value_col_end, we use `>` not `>=` for end check
        assert!(located.is_none());
    }

    #[test]
    fn find_action_at_start_of_value() {
        let yaml = "      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 14, // start of value
        };
        let located = find_action_at_position(yaml, pos).unwrap();
        assert_eq!(located.action_ref.repo(), "checkout");
    }

    #[test]
    fn repo_url_uses_default_host() {
        let action_ref = ActionRef::simple("actions", "checkout", "v4");
        assert_eq!(
            repo_url(&action_ref, Some("github.com")),
            "https://github.com/actions/checkout"
        );
    }

    #[test]
    fn repo_url_uses_explicit_host() {
        let action_ref = ActionRef::new(
            Some("ghe.mycorp.internal".to_owned()),
            String::from("platform"),
            String::from("ci-actions"),
            None,
            String::from("v1"),
        );
        assert_eq!(
            repo_url(&action_ref, Some("github.com")),
            "https://ghe.mycorp.internal/platform/ci-actions"
        );
    }

    #[test]
    fn repo_url_fallback_when_no_host() {
        let action_ref = ActionRef::simple("actions", "checkout", "v4");
        assert_eq!(
            repo_url(&action_ref, None),
            "https://github.com/actions/checkout"
        );
    }

    #[test]
    fn format_version_status_up_to_date() {
        assert_eq!(format_version_status(&CheckStatus::UpToDate), "Up to date");
    }

    #[test]
    fn format_version_status_outdated() {
        let target = Arc::new(version::Info::new(
            version::Version::parse("v2.0.0"),
            "abc1234567890abcdef1234567890abcdef123456",
        ));
        let status = CheckStatus::Outdated {
            current: "v1.0.0".into(),
            target,
        };
        let result = format_version_status(&status);
        assert!(result.contains("Outdated"));
        assert!(result.contains("v1.0.0"));
        assert!(result.contains("v2.0.0"));
        assert!(result.contains("abc1234"));
    }

    #[test]
    fn format_version_status_pinned_with_tag() {
        let status = CheckStatus::Pinned {
            sha: "abc1234567890abcdef1234567890abcdef123456".into(),
            tag: Some("v3.1.0".into()),
        };
        let result = format_version_status(&status);
        assert!(result.contains("Pinned"));
        assert!(result.contains("v3.1.0"));
        assert!(result.contains("abc123456789"));
    }

    #[test]
    fn format_version_status_pinned_unknown_tag() {
        let status = CheckStatus::Pinned {
            sha: "abc1234567890abcdef1234567890abcdef123456".into(),
            tag: None,
        };
        let result = format_version_status(&status);
        assert!(result.contains("unknown version"));
    }

    #[test]
    fn format_version_status_ignored() {
        assert_eq!(
            format_version_status(&CheckStatus::Ignored),
            "Ignored by config"
        );
    }

    #[test]
    fn format_version_status_unresolvable_user_error() {
        let status = CheckStatus::Unresolvable {
            reason: "repository not found".into(),
            is_environment_error: false,
        };
        let result = format_version_status(&status);
        assert!(result.contains("Unresolvable"));
        assert!(result.contains("repository not found"));
    }

    #[test]
    fn format_version_status_unresolvable_env_error() {
        let status = CheckStatus::Unresolvable {
            reason: "rate limited".into(),
            is_environment_error: true,
        };
        let result = format_version_status(&status);
        assert!(result.contains("Check failed"));
        assert!(result.contains("rate limited"));
    }

    #[test]
    fn build_hover_content_with_full_meta() {
        let action_ref = ActionRef::simple("actions", "checkout", "v3");
        let meta = CachedActionMeta {
            name: Some("Checkout".into()),
            description: Some("Checks out your repository.".into()),
            author: Some("GitHub".into()),
            deprecated_inputs: vec![("path".into(), "Use working-directory instead".into())],
            runtime: Some("node20".into()),
        };
        let status = CheckStatus::UpToDate;

        let content = build_hover_content(
            &action_ref,
            Some(&meta),
            &status,
            Some("v4.2.0"),
            Some("github.com"),
        );

        assert!(content.contains("### Checkout"));
        assert!(content.contains("Checks out your repository."));
        assert!(content.contains("*by GitHub*"));
        assert!(content.contains("`v3`"));
        assert!(content.contains("`v4.2.0`"));
        assert!(content.contains("Up to date"));
        assert!(content.contains("node20"));
        assert!(content.contains("https://github.com/actions/checkout"));
        assert!(content.contains("Deprecated inputs"));
        assert!(content.contains("`path`"));
        assert!(content.contains("Use working-directory instead"));
    }

    #[test]
    fn build_hover_content_without_meta() {
        let action_ref = ActionRef::simple("actions", "setup-node", "v4");
        let status = CheckStatus::UpToDate;

        let content = build_hover_content(&action_ref, None, &status, None, Some("github.com"));

        // Falls back to owner/repo as header.
        assert!(content.contains("### actions/setup-node"));
        assert!(content.contains("`v4`"));
        assert!(content.contains("Up to date"));
        assert!(content.contains("https://github.com/actions/setup-node"));
        // No deprecated inputs section.
        assert!(!content.contains("Deprecated inputs"));
    }

    #[test]
    fn build_hover_content_outdated_status() {
        let action_ref = ActionRef::simple("actions", "checkout", "v3");
        let target = Arc::new(version::Info::new(
            version::Version::parse("v4.2.0"),
            "abc1234567890abcdef1234567890abcdef123456",
        ));
        let status = CheckStatus::Outdated {
            current: "v3".into(),
            target,
        };

        let content = build_hover_content(
            &action_ref,
            None,
            &status,
            Some("v4.2.0"),
            Some("github.com"),
        );

        assert!(content.contains("Outdated"));
        assert!(content.contains("v3"));
        assert!(content.contains("v4.2.0"));
    }

    // --- compute_hover async tests ---

    #[tokio::test]
    async fn compute_hover_returns_hover_for_uses_line() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();

        let text = "      - uses: actions/checkout@v3\n";
        let position = Position {
            line: 0,
            character: 20,
        };

        let hover =
            compute_hover(text, position, &config, &registry, &file_cache, &meta_cache).await;
        let hover = hover.unwrap();

        match &hover.contents {
            HoverContents::Markup(mc) => {
                assert!(mc.value.contains("actions/checkout"));
                assert!(mc.value.contains("v3")); // current ref
            }
            other => panic!("expected Markup hover contents, got: {other:?}"),
        }

        // Range should cover the uses value.
        let range = hover.range.unwrap();
        assert_eq!(range.start.line, 0);
    }

    #[tokio::test]
    async fn compute_hover_returns_none_for_non_uses_line() {
        let provider = crate::test_utils::MockProvider::new("github.com");
        let registry = crate::test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();

        let text = "      - run: echo hello\n";
        let position = Position {
            line: 0,
            character: 10,
        };

        let hover =
            compute_hover(text, position, &config, &registry, &file_cache, &meta_cache).await;
        assert!(hover.is_none(), "expected no hover for non-uses line");
    }

    #[tokio::test]
    async fn compute_hover_shows_outdated_status() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();

        let text = "      - uses: actions/checkout@v3\n";
        let position = Position {
            line: 0,
            character: 20,
        };

        let hover = compute_hover(text, position, &config, &registry, &file_cache, &meta_cache)
            .await
            .unwrap();

        match &hover.contents {
            HoverContents::Markup(mc) => {
                assert!(
                    mc.value.contains("Outdated"),
                    "expected 'Outdated' in hover, got: {}",
                    mc.value
                );
                assert!(mc.value.contains("v4.2.0"));
            }
            other => panic!("expected Markup, got: {other:?}"),
        }
    }

    #[test]
    fn action_meta_cache_stores_and_retrieves() {
        let cache = new_action_meta_cache();
        let key = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v4".into(),
            host: None,
        };
        let meta = Arc::new(CachedActionMeta {
            name: Some("Checkout".into()),
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        });
        cache.insert(key.clone(), Arc::clone(&meta));
        assert!(cache.get(&key).is_some());
        assert_eq!(cache.get(&key).unwrap().name.as_deref(), Some("Checkout"));
    }

    #[test]
    fn action_meta_cache_key_distinguishes_refs() {
        let cache = new_action_meta_cache();
        let key_v3 = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v3".into(),
            host: None,
        };
        let key_v4 = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v4".into(),
            host: None,
        };
        let meta = Arc::new(CachedActionMeta {
            name: Some("V3".into()),
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        });
        cache.insert(key_v3.clone(), meta);
        assert!(cache.get(&key_v3).is_some());
        assert!(cache.get(&key_v4).is_none());
    }
}
